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
///
/// Sorted by `(start, end)`, not `start` alone: a zero-width insert and a real (non-empty) range
/// that both begin at the same offset are allowed to *touch* (the overlap check below is `<=`,
/// not `<`), but only in one order — the insert must land before the range that starts there, not
/// after. Keying on `start` alone leaves that order to `sort_by_key`'s tie-breaking, which is
/// stable (preserves whichever order the caller happened to push them in) — correct only by
/// accident, and silently wrong (a spurious panic) the moment two independent edit-emitting code
/// paths push such a pair in the other order. Ordering shorter ranges first at a shared start
/// makes the zero-width case sort correctly regardless of push order.
pub fn apply_edits(source: &str, mut edits: Vec<TextEdit>) -> String {
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by_key(|e| (e.range.start(), e.range.end()));
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

    #[test]
    fn a_zero_width_insert_and_a_range_starting_at_the_same_offset_do_not_panic_regardless_of_push_order() {
        // Regression: sorting by `start` alone leaves the relative order of a same-start pair to
        // `sort_by_key`'s tie-breaking (stable — whichever push order the caller happened to use)
        // instead of the only order that's actually valid here (the zero-width insert has to land
        // *before* the range that starts there, not after) — real callers push these from two
        // independent code paths with no coordination between them, so relying on push order is
        // fragile. Both push orders must produce the identical, correct result.
        let source = "hello world";
        let insert = TextEdit::new(range(5, 5), "!");
        let delete = TextEdit::new(range(5, 11), "");

        let insert_pushed_first = apply_edits(source, vec![insert.clone(), delete.clone()]);
        let delete_pushed_first = apply_edits(source, vec![delete, insert]);

        assert_eq!(insert_pushed_first, "hello!");
        assert_eq!(delete_pushed_first, "hello!");
    }
}
