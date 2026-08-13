//! Makes it visible, directly in the Python source, which ancestor class a given auto-managed
//! (inherited, non-overridden) parameter's documentation actually came from — two selectable
//! presentation styles, both driven by the same stamped `style::EntryOrigin` on each `ParamEntry`
//! (see `sync.rs`'s `resolve_and_rewrite`, which stamps it once per class and relies on its own
//! "never overwrite a pass-through entry" invariant to keep it correct at any inheritance depth):
//!
//! - **Comment mode**: a tool-managed `# docerator: provenance` block inserted right after the
//!   docstring, summarizing which ancestor(s) contributed which parameters. This module owns
//!   detecting, rendering, and reconciling that block (`find_existing_block`/`render_block`/
//!   `reconcile`) — the one genuinely new idempotency mechanism this feature needs, since a
//!   comment isn't part of any structure the engine already round-trips through parsing.
//! - **Inline mode**: a provenance note appended directly under a copied parameter's own
//!   rendered text (`append_inline_note`). This needs no dedicated idempotency mechanism at all —
//!   it's naturally covered by the "render fresh, diff against on-disk, edit only if different"
//!   pattern every other auto-managed edit in this engine already uses.

use std::str::FromStr;

use ruff_text_size::{TextRange, TextSize};

use indexmap::IndexMap;

use crate::edit::TextEdit;
use crate::style::EntryOrigin;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProvenanceMode {
    Off,
    #[default]
    Comment,
    Inline,
}

impl FromStr for ProvenanceMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(ProvenanceMode::Off),
            "comment" => Ok(ProvenanceMode::Comment),
            "inline" => Ok(ProvenanceMode::Inline),
            other => Err(format!("'{other}' is not a valid provenance mode (expected one of: off, comment, inline)")),
        }
    }
}

/// One auto-managed parameter, ready to be summarized by `render_block` — a name plus the class
/// that really authored it.
pub struct ProvenanceEntry {
    pub name: String,
    pub origin: EntryOrigin,
}

const HEADER_LINE: &str = "# docerator: provenance";
const FROM_PREFIX: &str = "# docerator: from ";

/// Splits the physical line starting at `start` into `(content_end, next_line_start)` —
/// `content_end` excludes any `\r\n`/`\n` terminator (mirrors `style::numpydoc::Line`'s own rule:
/// a `\r\n` line's `\r` is never treated as part of its content); `next_line_start` is `None`
/// when this is the source's last line (no further `\n` to step past).
fn split_one_line(source: &str, start: usize) -> (usize, Option<usize>) {
    let bytes = source.as_bytes();
    match bytes[start..].iter().position(|&b| b == b'\n') {
        Some(offset) => {
            let raw_end = start + offset;
            let content_end = if raw_end > start && bytes[raw_end - 1] == b'\r' { raw_end - 1 } else { raw_end };
            (content_end, Some(raw_end + 1))
        }
        None => (source.len(), None),
    }
}

/// Whether `literal_end` (a docstring's own closing-quote position) is genuinely alone on its own
/// physical line — nothing but optional trailing whitespace before the next `\n`. Python permits
/// semicolon-chaining another statement onto the same line as a string-literal statement
/// (`"""Doc."""; other_statement()`); inserting a `#` comment right at `literal_end` in that case
/// would silently comment out everything after it on that line, so this guards that hazard rather
/// than trying to handle it.
fn literal_alone_on_its_line(source: &str, literal_end: TextSize) -> bool {
    let start = usize::from(literal_end);
    if start > source.len() {
        return false;
    }
    let (line_content_end, _) = split_one_line(source, start);
    source[start..line_content_end].trim().is_empty()
}

/// Downward scan for an existing managed provenance block, starting right after `after`
/// (a docstring literal's own end) — mirrors `directives.rs`'s upward-scan-for-directives, but
/// inverted: the block, if any, starts on the *next* physical line (the literal's closing quotes
/// share their own line with nothing useful to scan), skipping over any number of leading blank
/// lines first (a code formatter run between docerator runs may have inserted one or more between
/// the docstring and a block docerator itself wrote — tolerating them here is what keeps that from
/// making the block invisible and getting a duplicate inserted right on top of it; see
/// `reconcile`'s delete case for why the blank lines never need special handling of their own
/// beyond this). The first non-blank line must be an exact (whitespace-trimmed) match for
/// `HEADER_LINE` to count as ours; every contiguous following line whose trimmed content starts
/// with `FROM_PREFIX` extends the block. Anything else — a second blank line inside the block, an
/// unrelated comment, real code — stops the scan; if it's the first non-blank line, there's no
/// block here at all. Returns the block's own content range, blank lines excluded (no trailing
/// terminator on its last line either, matching how `render_block`'s own output is shaped, so the
/// two can be compared byte-for-byte).
pub(crate) fn find_existing_block(source: &str, after: TextSize) -> Option<TextRange> {
    let after = usize::from(after);
    if after > source.len() {
        return None;
    }
    let (_, next) = split_one_line(source, after);
    let mut pos = next?;

    loop {
        let (line_end, next_cursor) = split_one_line(source, pos);
        if !source[pos..line_end].trim().is_empty() {
            break;
        }
        pos = next_cursor?;
    }

    let (header_end, mut cursor) = split_one_line(source, pos);
    if source[pos..header_end].trim() != HEADER_LINE {
        return None;
    }
    let block_start = pos;
    let mut block_end = header_end;

    while let Some(next_pos) = cursor {
        pos = next_pos;
        let (line_end, next_cursor) = split_one_line(source, pos);
        if !source[pos..line_end].trim().starts_with(FROM_PREFIX) {
            break;
        }
        block_end = line_end;
        cursor = next_cursor;
    }

    Some(TextRange::new(TextSize::try_from(block_start).unwrap(), TextSize::try_from(block_end).unwrap()))
}

/// Renders the managed provenance comment block for `entries`, grouped by origin (first-
/// appearance order preserved, so the block's own line order matches the entries' signature
/// order) — empty string when `entries` is empty, meaning "no block should exist here at all".
/// Never ends in a trailing newline, matching `find_existing_block`'s own content-only range, so
/// the two can be diffed directly.
pub(crate) fn render_block(entries: &[ProvenanceEntry], indent: &str, newline: &str) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut by_origin: IndexMap<&EntryOrigin, Vec<&str>> = IndexMap::new();
    for entry in entries {
        by_origin.entry(&entry.origin).or_default().push(entry.name.as_str());
    }
    let mut lines: Vec<String> = vec![format!("{indent}{HEADER_LINE}")];
    for (origin, names) in &by_origin {
        lines.push(format!("{indent}{FROM_PREFIX}{}: {}", origin.display(), names.join(", ")));
    }
    lines.join(newline)
}

pub(crate) enum ReconcileOutcome {
    /// Already correct (or nothing was ever wanted here) — no edit needed.
    NoOp,
    Edit(TextEdit),
    /// A block was wanted (or needed cleanup) but `literal_end` isn't alone on its own line, so
    /// inserting a comment there would risk corrupting a semicolon-chained statement — diagnose
    /// instead of touching it.
    Blocked,
}

/// Diffs the desired provenance block (from `entries`) against whatever's already on disk right
/// after `literal_end`, and returns the one edit needed to reconcile them.
pub(crate) fn reconcile(source: &str, literal_end: TextSize, indent: &str, newline: &str, entries: &[ProvenanceEntry]) -> ReconcileOutcome {
    let desired = render_block(entries, indent, newline);

    if !literal_alone_on_its_line(source, literal_end) {
        return if desired.is_empty() { ReconcileOutcome::NoOp } else { ReconcileOutcome::Blocked };
    }

    let existing = find_existing_block(source, literal_end);

    match (existing, desired.is_empty()) {
        (None, true) => ReconcileOutcome::NoOp,
        (None, false) => {
            ReconcileOutcome::Edit(TextEdit::new(TextRange::new(literal_end, literal_end), format!("{newline}{desired}")))
        }
        (Some(range), true) => {
            // Delete the block AND the newline that introduced it (the docstring's own line's
            // terminator), so the docstring's line reconnects directly to whatever follows —
            // never leaving a dangling blank line behind.
            let (line_content_end, _) = split_one_line(source, usize::from(literal_end));
            let from = TextSize::try_from(line_content_end).unwrap();
            ReconcileOutcome::Edit(TextEdit::new(TextRange::new(from, range.end()), String::new()))
        }
        (Some(range), false) => {
            let current = &source[usize::from(range.start())..usize::from(range.end())];
            if current == desired {
                ReconcileOutcome::NoOp
            } else {
                ReconcileOutcome::Edit(TextEdit::new(range, desired))
            }
        }
    }
}

/// Appends a trailing provenance note under a copied entry's own already-rendered (and, at every
/// real call site, already raw-ness-safety-checked) text — its own new line, indented to the
/// `name : type` line's own margin plus numpydoc's fixed 4-space description-nesting convention,
/// matching how a real multi-line description is already indented. A no-op passthrough when
/// `mode` isn't `Inline`, or when `origin` is unexpectedly absent (never panics).
pub(crate) fn maybe_append_inline_note(rendered: String, margin_indent: &str, newline: &str, mode: ProvenanceMode, origin: Option<&EntryOrigin>) -> String {
    match (mode, origin) {
        (ProvenanceMode::Inline, Some(origin)) => {
            format!("{rendered}{newline}{margin_indent}    (Inherited from {}.)", origin.display())
        }
        _ => rendered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(module: &str, class_name: &str) -> EntryOrigin {
        EntryOrigin {
            module: module.to_string(),
            class_name: class_name.to_string(),
        }
    }

    #[test]
    fn render_block_is_empty_for_no_entries() {
        assert_eq!(render_block(&[], "    ", "\n"), "");
    }

    #[test]
    fn render_block_groups_by_origin_preserving_first_appearance_order() {
        let entries = vec![
            ProvenanceEntry {
                name: "location".to_string(),
                origin: origin("pkg.survey", "BaseSrc"),
            },
            ProvenanceEntry {
                name: "frequency".to_string(),
                origin: origin("pkg.fdem", "BaseFDEMSrc"),
            },
            ProvenanceEntry {
                name: "receiver_list".to_string(),
                origin: origin("pkg.survey", "BaseSrc"),
            },
        ];
        let rendered = render_block(&entries, "    ", "\n");
        assert_eq!(
            rendered,
            "    # docerator: provenance\n    \
             # docerator: from pkg.survey.BaseSrc: location, receiver_list\n    \
             # docerator: from pkg.fdem.BaseFDEMSrc: frequency"
        );
    }

    #[test]
    fn find_existing_block_detects_a_well_formed_block() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"
    # docerator: provenance
    # docerator: from pkg.Base: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let range = find_existing_block(source, literal_end).expect("block should be found");
        let content = &source[usize::from(range.start())..usize::from(range.end())];
        assert_eq!(content, "    # docerator: provenance\n    # docerator: from pkg.Base: arg1");
    }

    #[test]
    fn find_existing_block_returns_none_for_an_unrelated_comment() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"
    # TODO: something unrelated

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        assert_eq!(find_existing_block(source, literal_end), None);
    }

    #[test]
    fn find_existing_block_stops_at_a_non_matching_line() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"
    # docerator: provenance
    # docerator: from pkg.Base: arg1
    # a hand-written comment right after, not ours

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let range = find_existing_block(source, literal_end).expect("block should be found");
        let content = &source[usize::from(range.start())..usize::from(range.end())];
        assert_eq!(content, "    # docerator: provenance\n    # docerator: from pkg.Base: arg1");
    }

    #[test]
    fn find_existing_block_tolerates_a_blank_line_a_formatter_inserted_before_the_header() {
        // A code formatter (Black, ruff format, ...) may insert a blank line between a class
        // docstring and whatever follows it, including a block docerator itself wrote on a
        // previous run -- that blank line must not make the block invisible.
        let source = "\
class Child:
    \"\"\"Child.\"\"\"

    # docerator: provenance
    # docerator: from pkg.Base: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let range = find_existing_block(source, literal_end).expect("block should be found despite the blank line");
        let content = &source[usize::from(range.start())..usize::from(range.end())];
        assert_eq!(content, "    # docerator: provenance\n    # docerator: from pkg.Base: arg1");
    }

    #[test]
    fn find_existing_block_tolerates_several_blank_lines_before_the_header() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"


    # docerator: provenance
    # docerator: from pkg.Base: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let range = find_existing_block(source, literal_end).expect("block should be found despite two blank lines");
        let content = &source[usize::from(range.start())..usize::from(range.end())];
        assert_eq!(content, "    # docerator: provenance\n    # docerator: from pkg.Base: arg1");
    }

    #[test]
    fn find_existing_block_still_returns_none_when_blank_lines_precede_unrelated_content() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"

    # TODO: something unrelated

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        assert_eq!(find_existing_block(source, literal_end), None);
    }

    #[test]
    fn reconcile_is_a_noop_when_a_formatter_inserted_a_blank_line_before_an_already_correct_block() {
        // The actual regression this guards: without blank-line tolerance, `reconcile` would
        // think no block exists here at all and insert a second one right on top of the real one.
        let source = "\
class Child:
    \"\"\"Child.\"\"\"

    # docerator: provenance
    # docerator: from pkg.Base: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let entries = vec![ProvenanceEntry {
            name: "arg1".to_string(),
            origin: origin("pkg", "Base"),
        }];
        assert!(matches!(reconcile(source, literal_end, "    ", "\n", &entries), ReconcileOutcome::NoOp));
    }

    #[test]
    fn reconcile_replaces_preserving_a_formatter_inserted_blank_line_gap() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"

    # docerator: provenance
    # docerator: from pkg.OldBase: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let entries = vec![ProvenanceEntry {
            name: "arg1".to_string(),
            origin: origin("pkg", "NewBase"),
        }];
        match reconcile(source, literal_end, "    ", "\n", &entries) {
            ReconcileOutcome::Edit(edit) => {
                let mut result = source.to_string();
                result.replace_range(usize::from(edit.range.start())..usize::from(edit.range.end()), &edit.replacement);
                // The blank line before the block, presumably left by a formatter, survives
                // untouched -- only the block's own content changed.
                assert_eq!(
                    result,
                    "class Child:\n    \"\"\"Child.\"\"\"\n\n    # docerator: provenance\n    \
                     # docerator: from pkg.NewBase: arg1\n\n    def __init__(self, arg1):\n        pass\n"
                );
            }
            _ => panic!("expected a replace edit"),
        }
    }

    #[test]
    fn reconcile_deletes_the_block_and_the_gap_a_formatter_left_before_it() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"

    # docerator: provenance
    # docerator: from pkg.Base: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        match reconcile(source, literal_end, "    ", "\n", &[]) {
            ReconcileOutcome::Edit(edit) => {
                let mut result = source.to_string();
                result.replace_range(usize::from(edit.range.start())..usize::from(edit.range.end()), &edit.replacement);
                // Restores exactly the gap that would have separated the docstring from `def` if
                // the block had never been there -- not a leftover double-blank-line artifact.
                assert_eq!(result, "class Child:\n    \"\"\"Child.\"\"\"\n\n    def __init__(self, arg1):\n        pass\n");
            }
            _ => panic!("expected a delete edit"),
        }
    }

    #[test]
    fn reconcile_inserts_when_absent_and_desired() {
        let source = "class Child:\n    \"\"\"Child.\"\"\"\n\n    def __init__(self, arg1):\n        pass\n";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let entries = vec![ProvenanceEntry {
            name: "arg1".to_string(),
            origin: origin("pkg", "Base"),
        }];
        match reconcile(source, literal_end, "    ", "\n", &entries) {
            ReconcileOutcome::Edit(edit) => {
                assert_eq!(edit.range, TextRange::new(literal_end, literal_end));
                assert_eq!(edit.replacement, "\n    # docerator: provenance\n    # docerator: from pkg.Base: arg1");
            }
            _ => panic!("expected an insert edit"),
        }
    }

    #[test]
    fn reconcile_deletes_when_no_longer_needed() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"
    # docerator: provenance
    # docerator: from pkg.Base: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        match reconcile(source, literal_end, "    ", "\n", &[]) {
            ReconcileOutcome::Edit(edit) => {
                let mut result = source.to_string();
                result.replace_range(usize::from(edit.range.start())..usize::from(edit.range.end()), &edit.replacement);
                assert_eq!(result, "class Child:\n    \"\"\"Child.\"\"\"\n\n    def __init__(self, arg1):\n        pass\n");
            }
            _ => panic!("expected a delete edit"),
        }
    }

    #[test]
    fn reconcile_is_a_noop_when_already_correct() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"
    # docerator: provenance
    # docerator: from pkg.Base: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let entries = vec![ProvenanceEntry {
            name: "arg1".to_string(),
            origin: origin("pkg", "Base"),
        }];
        assert!(matches!(reconcile(source, literal_end, "    ", "\n", &entries), ReconcileOutcome::NoOp));
    }

    #[test]
    fn reconcile_replaces_when_origins_changed() {
        let source = "\
class Child:
    \"\"\"Child.\"\"\"
    # docerator: provenance
    # docerator: from pkg.OldBase: arg1

    def __init__(self, arg1):
        pass
";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let entries = vec![ProvenanceEntry {
            name: "arg1".to_string(),
            origin: origin("pkg", "NewBase"),
        }];
        match reconcile(source, literal_end, "    ", "\n", &entries) {
            ReconcileOutcome::Edit(edit) => {
                assert_eq!(edit.replacement, "    # docerator: provenance\n    # docerator: from pkg.NewBase: arg1");
            }
            _ => panic!("expected a replace edit"),
        }
    }

    #[test]
    fn reconcile_is_blocked_by_a_semicolon_chained_statement_on_the_docstring_s_own_line() {
        let source = "class Child:\n    \"\"\"Child.\"\"\"; x = 1\n\n    def __init__(self, arg1):\n        pass\n";
        let literal_end = TextSize::try_from(source.find("\"\"\"Child.\"\"\"").unwrap() + "\"\"\"Child.\"\"\"".len()).unwrap();
        let entries = vec![ProvenanceEntry {
            name: "arg1".to_string(),
            origin: origin("pkg", "Base"),
        }];
        assert!(matches!(reconcile(source, literal_end, "    ", "\n", &entries), ReconcileOutcome::Blocked));
    }

    #[test]
    fn append_inline_note_uses_the_margin_plus_four_spaces() {
        let rendered = "arg1 : int\n    Description.";
        let note = maybe_append_inline_note(rendered.to_string(), "    ", "\n", ProvenanceMode::Inline, Some(&origin("pkg", "Base")));
        assert_eq!(note, "arg1 : int\n    Description.\n        (Inherited from pkg.Base.)");
    }

    #[test]
    fn append_inline_note_is_a_noop_outside_inline_mode() {
        let rendered = "arg1 : int\n    Description.".to_string();
        let unchanged = maybe_append_inline_note(rendered.clone(), "    ", "\n", ProvenanceMode::Comment, Some(&origin("pkg", "Base")));
        assert_eq!(unchanged, rendered);
    }

    #[test]
    fn provenance_mode_from_str_is_case_insensitive_and_rejects_unknown_values() {
        assert_eq!("off".parse(), Ok(ProvenanceMode::Off));
        assert_eq!("Comment".parse(), Ok(ProvenanceMode::Comment));
        assert_eq!("INLINE".parse::<ProvenanceMode>().unwrap(), ProvenanceMode::Inline);
        assert!("bogus".parse::<ProvenanceMode>().is_err());
    }
}
