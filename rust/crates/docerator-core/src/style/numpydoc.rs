//! NumPy-style ("numpydoc") docstring parsing — the only [`DocStyle`] implementation for now.
//!
//! A hand-written line scanner rather than a regex port (the original Python parser,
//! `docerator/parsers/_numpydoc.py`, used one big chained-optional regex): a scanner gives
//! precise `file:line:col`-capable diagnostics (distinguishing "wrong section order" from
//! "wrong underline length"), which a single opaque regex match/no-match cannot, and it
//! naturally produces the per-entry byte ranges the auto-sync engine needs to splice text —
//! the original never needed ranges since it worked on decoded Python `str` objects, not
//! source bytes.

use indexmap::IndexMap;
use ruff_text_size::{TextRange, TextSize};

use super::{Diagnostic, DocStyle, ParamEntry, ParamSectionKind, ParsedEntries, Severity};

/// Canonical numpydoc section order. A section recognized out of this order relative to an
/// already-recognized section is rejected (not treated as a section boundary at all) — this
/// mirrors the original's fixed-order chained regex, which simply couldn't match sections out
/// of sequence.
const SECTIONS: &[&str] = &[
    "Parameters",
    "Attributes",
    "Methods",
    "Returns",
    "Yields",
    "Receives",
    "Other Parameters",
    "Raises",
    "Warns",
    "Warnings",
    "See Also",
    "Notes",
    "References",
    "Examples",
    "index",
];

const PARAMETERS_INDEX: usize = 0;
const OTHER_PARAMETERS_INDEX: usize = 6;

pub struct NumpydocStyle;

impl DocStyle for NumpydocStyle {
    fn parse_entries(&self, docstring_text: &str) -> (ParsedEntries, Vec<Diagnostic>) {
        parse_entries(docstring_text)
    }

    fn format_entry(&self, entry: &ParamEntry, _indent: &str) -> String {
        // `description` is captured verbatim from wherever it was copied from, including its
        // own leading whitespace on continuation lines — correct as-is when source and target
        // share indentation context, which is true for every M2 fixture (same file, same
        // nesting depth). Re-anchoring a description copied across a *different* indentation
        // context (nested class, nested nesting depth, cross-file) is not implemented yet;
        // `indent` is accepted now so the call sites don't need to change when that lands.
        let mut out = entry.name.clone();
        if let Some(ty) = &entry.type_description {
            out.push_str(" : ");
            out.push_str(ty);
        }
        if let Some(desc) = &entry.description {
            out.push('\n');
            out.push_str(desc);
        }
        out
    }

    fn synthesize_section(&self, _section: ParamSectionKind) -> String {
        unimplemented!("wired up when M3+ needs to create a Parameters section from scratch")
    }
}

/// One physical line's content span within the parsed text, terminator excluded (a trailing
/// `\r` is stripped too, so CRLF- and LF-terminated input are treated identically — a
/// deliberate simplification; docstring content essentially never depends on this).
#[derive(Debug, Clone, Copy)]
struct Line {
    start: usize,
    end: usize,
}

impl Line {
    fn text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }

    fn indent(&self, source: &str) -> usize {
        indent_of(self.text(source))
    }
}

fn indent_of(s: &str) -> usize {
    s.as_bytes()
        .iter()
        .take_while(|&&b| b == b' ' || b == b'\t')
        .count()
}

fn split_lines(text: &str) -> Vec<Line> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(make_line(text, start, i));
            start = i + 1;
        }
    }
    lines.push(make_line(text, start, bytes.len()));
    lines
}

fn make_line(text: &str, start: usize, mut end: usize) -> Line {
    if end > start && text.as_bytes()[end - 1] == b'\r' {
        end -= 1;
    }
    Line { start, end }
}

/// The indentation shared by every line after the first (the summary line is exempt, matching
/// `inspect.cleandoc`'s treatment of it) — the "logical column 0" that section headers and
/// arg-name lines must sit at. Blank lines never count toward the minimum.
fn compute_margin(lines: &[Line], source: &str) -> usize {
    let mut margin = usize::MAX;
    for line in lines.iter().skip(1) {
        let text = line.text(source);
        if !text.trim().is_empty() {
            margin = margin.min(indent_of(text));
        }
    }
    if margin == usize::MAX {
        0
    } else {
        margin
    }
}

struct SectionBody {
    canon_index: usize,
    body_start: usize,
    body_end: usize,
}

/// Scan for canonically-ordered `<Name>\n<dashes>\n` section headers, returning each
/// recognized section's body byte range (the text strictly between its underline and the next
/// recognized header, or end of input for the last one).
fn find_sections(lines: &[Line], source: &str, margin: usize, diagnostics: &mut Vec<Diagnostic>) -> Vec<SectionBody> {
    let mut accepted: Vec<(usize, usize)> = Vec::new(); // (canon_index, header_line_idx)
    let mut last_index: Option<usize> = None;
    let mut i = 0usize;
    while i < lines.len() {
        let line = &lines[i];
        if line.indent(source) == margin {
            let trimmed = &line.text(source)[margin..];
            if let Some(canon_index) = SECTIONS.iter().position(|s| *s == trimmed) {
                if let Some(next) = lines.get(i + 1) {
                    if next.indent(source) == margin {
                        let next_trimmed = &next.text(source)[margin..];
                        if !next_trimmed.is_empty() && next_trimmed.bytes().all(|b| b == b'-') {
                            let expected_len = SECTIONS[canon_index].chars().count();
                            if next_trimmed.chars().count() == expected_len {
                                if last_index.is_none_or(|li| canon_index > li) {
                                    accepted.push((canon_index, i));
                                    last_index = Some(canon_index);
                                } else {
                                    diagnostics.push(Diagnostic {
                                        code: "DOC003",
                                        severity: Severity::Error,
                                        message: format!(
                                            "'{}' section appears out of the expected numpydoc section order",
                                            SECTIONS[canon_index]
                                        ),
                                        range: line_range(line),
                                    });
                                }
                                i += 2;
                                continue;
                            } else {
                                diagnostics.push(Diagnostic {
                                    code: "DOC004",
                                    severity: Severity::Error,
                                    message: format!(
                                        "'{}' section underline must be exactly {} dashes, found {}",
                                        SECTIONS[canon_index],
                                        expected_len,
                                        next_trimmed.chars().count()
                                    ),
                                    range: line_range(next),
                                });
                                i += 2;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }

    // The last recognized section's body otherwise runs to the raw input's exact end — but a
    // docstring's raw interior text almost always has trailing "\n<indent>" before the closing
    // quotes, which would otherwise get captured as part of the final entry's description.
    // Trimming trailing whitespace here (only ever shortens the end offset, so earlier byte
    // offsets stay valid) mirrors `inspect.cleandoc`'s trailing-blank-line trim.
    let trimmed_end = source.trim_end().len();

    let mut sections = Vec::with_capacity(accepted.len());
    for (idx, &(canon_index, header_idx)) in accepted.iter().enumerate() {
        let body_start = lines
            .get(header_idx + 2)
            .map(|l| l.start)
            .unwrap_or(lines[header_idx + 1].end);
        let body_end = if idx + 1 < accepted.len() {
            lines[accepted[idx + 1].1].start.saturating_sub(1)
        } else {
            trimmed_end
        };
        sections.push(SectionBody {
            canon_index,
            body_start,
            body_end,
        });
    }
    sections
}

fn line_range(line: &Line) -> TextRange {
    TextRange::new(
        TextSize::try_from(line.start).unwrap(),
        TextSize::try_from(line.end).unwrap(),
    )
}

/// An arg-name line found within a section body — column-`margin`, non-blank content, whatever
/// follows an optional `: type` suffix. Mirrors `NUMPY_ARG_TYPE_REGEX`.
fn collect_arg_lines(lines: &[Line], source: &str, body_start: usize, body_end: usize, margin: usize, out: &mut Vec<usize>) {
    for (idx, line) in lines.iter().enumerate() {
        if line.start < body_start || line.end > body_end {
            continue;
        }
        if line.indent(source) != margin {
            continue;
        }
        let content = &line.text(source)[margin..];
        if content.is_empty() {
            continue;
        }
        out.push(idx);
    }
}

/// Split `content` on its first `:` — mirrors the non-greedy `arg_name` / optional `type`
/// regex groups, which always prefer the earliest colon.
fn split_name_type(content: &str) -> (&str, Option<&str>) {
    match content.find(':') {
        Some(pos) => {
            let name = content[..pos].trim_end();
            let type_desc = content[pos + 1..].trim_start();
            (name, if type_desc.is_empty() { None } else { Some(type_desc) })
        }
        None => (content, None),
    }
}

fn parse_entries(source: &str) -> (ParsedEntries, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let lines = split_lines(source);
    let margin = compute_margin(&lines, source);
    let sections = find_sections(&lines, source, margin, &mut diagnostics);

    let params_section = sections.iter().find(|s| s.canon_index == PARAMETERS_INDEX);
    let others_section = sections.iter().find(|s| s.canon_index == OTHER_PARAMETERS_INDEX);

    let mut arg_line_indices: Vec<usize> = Vec::new();
    if let Some(s) = params_section {
        collect_arg_lines(&lines, source, s.body_start, s.body_end, margin, &mut arg_line_indices);
    }
    if let Some(s) = others_section {
        collect_arg_lines(&lines, source, s.body_start, s.body_end, margin, &mut arg_line_indices);
    }

    let mut entries = IndexMap::new();
    for (pos, &line_idx) in arg_line_indices.iter().enumerate() {
        let line = &lines[line_idx];
        let content = &line.text(source)[margin..];
        if content.starts_with('*') {
            // *args / **kwargs — matched so boundary slicing above/below stays correct,
            // but never documented as a real parameter.
            continue;
        }
        let (raw_names, type_desc) = split_name_type(content);

        let section_end = if let Some(s) = params_section {
            if line.start < s.body_end {
                s.body_end
            } else {
                others_section.map(|o| o.body_end).unwrap_or(s.body_end)
            }
        } else {
            others_section.map(|o| o.body_end).unwrap_or(line.end)
        };

        let desc_start = line.end + 1;
        let desc_end = match arg_line_indices.get(pos + 1) {
            Some(&next_idx) => lines[next_idx].start.saturating_sub(1).min(section_end),
            None => section_end,
        };
        let description = if desc_start < desc_end {
            let text = &source[desc_start..desc_end];
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        } else {
            None
        };

        // Range starts *after* the line's leading margin indentation, not at the line start —
        // the indentation is untouched surrounding text, not part of the entry's own content,
        // so splicing a regenerated entry never clobbers it.
        let entry_range = TextRange::new(
            TextSize::try_from(line.start + margin).unwrap(),
            TextSize::try_from(desc_end.max(line.end)).unwrap(),
        );

        for name in raw_names.split(',').map(str::trim) {
            entries.insert(
                name.to_string(),
                ParamEntry {
                    name: name.to_string(),
                    type_description: type_desc.map(str::to_string),
                    description: description.clone(),
                    range: entry_range,
                },
            );
        }
    }

    if let Some(s) = params_section {
        let params_only_lines: Vec<usize> = arg_line_indices
            .iter()
            .copied()
            .filter(|&idx| lines[idx].start >= s.body_start && lines[idx].end <= s.body_end)
            .collect();
        if params_only_lines.is_empty() {
            diagnostics.push(Diagnostic {
                code: "DOC005",
                severity: Severity::Warning,
                message: "'Parameters' section found but no arguments were parsed under it — \
                          check that argument names are at the same indentation as the section heading"
                    .to_string(),
                range: TextRange::new(
                    TextSize::try_from(s.body_start).unwrap(),
                    TextSize::try_from(s.body_start).unwrap(),
                ),
            });
        }
    }

    (ParsedEntries { entries }, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn entry_names(parsed: &ParsedEntries) -> Vec<&str> {
        parsed.entries.keys().map(String::as_str).collect()
    }

    #[rstest]
    #[case("item : type", "item", Some("type"))]
    #[case("item: type", "item", Some("type"))]
    #[case("item :type", "item", Some("type"))]
    #[case("item:type", "item", Some("type"))]
    #[case("item : bool, default:True", "item", Some("bool, default:True"))]
    #[case("item3 :other no space", "item3", Some("other no space"))]
    #[case("item_no_type", "item_no_type", None)]
    fn splits_name_and_type_on_first_colon(#[case] content: &str, #[case] name: &str, #[case] ty: Option<&str>) {
        assert_eq!(split_name_type(content), (name, ty));
    }

    #[test]
    fn parses_full_parameter_section_case_table() {
        let doc = "Summary\n\nParameters\n----------\nitem_no_type\nitem1 : type\nitem2_no_space: object, optional\nitem3 :other no space\nitem4\n    I've got a 1 line description\nitem5\n    I've got a 2 line\n    description\nitem6 : type\n    I've got a description line\n\n    that has an empty line in it.\nitem7 : type\n    I've got a description line\n    that ends with an empty line.\n\nmultiple, args : shared type\n    Shared Description\n\nExamples\n--------\nnothing here\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

        assert_eq!(
            entry_names(&parsed),
            vec![
                "item_no_type",
                "item1",
                "item2_no_space",
                "item3",
                "item4",
                "item5",
                "item6",
                "item7",
                "multiple",
                "args",
            ]
        );

        let get = |n: &str| parsed.entries.get(n).unwrap();
        assert_eq!(get("item_no_type").type_description, None);
        assert_eq!(get("item1").type_description.as_deref(), Some("type"));
        assert_eq!(get("item2_no_space").type_description.as_deref(), Some("object, optional"));
        assert_eq!(get("item3").type_description.as_deref(), Some("other no space"));

        assert_eq!(get("item4").description.as_deref(), Some("    I've got a 1 line description"));
        assert_eq!(get("item5").description.as_deref(), Some("    I've got a 2 line\n    description"));
        assert_eq!(
            get("item6").description.as_deref(),
            Some("    I've got a description line\n\n    that has an empty line in it.")
        );
        assert_eq!(
            get("item7").description.as_deref(),
            Some("    I've got a description line\n    that ends with an empty line.\n")
        );
        assert_eq!(get("multiple").type_description.as_deref(), Some("shared type"));
        assert_eq!(get("multiple").description.as_deref(), Some("    Shared Description\n"));
        assert_eq!(get("args").type_description.as_deref(), Some("shared type"));
        assert_eq!(get("args").description.as_deref(), Some("    Shared Description\n"));
    }

    #[test]
    fn skips_star_args_and_kwargs() {
        let doc = "Summary\n\nParameters\n----------\nreal_arg : int\n    Documented.\n*args\n**kwargs\n";
        let (parsed, _diagnostics) = parse_entries(doc);
        assert_eq!(entry_names(&parsed), vec!["real_arg"]);
    }

    #[test]
    fn other_parameters_section_is_included() {
        let doc = "Summary\n\nParameters\n----------\nfirst : int\n    First.\n\nOther Parameters\n----------------\nsecond : str\n    Second.\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(diagnostics.is_empty());
        assert_eq!(entry_names(&parsed), vec!["first", "second"]);
    }

    #[test]
    fn section_out_of_canonical_order_is_rejected_with_diagnostic() {
        let doc = "Summary\n\nAttributes\n----------\nitem1\n\nParameters\n----------\nitem2\n\nReturns\n-------\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(parsed.entries.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC003");
    }

    #[rstest]
    #[case(3)]
    #[case(50)]
    fn wrong_underline_length_is_rejected_with_diagnostic(#[case] dash_length: usize) {
        let dashes = "-".repeat(dash_length);
        let doc = format!("Summary\n\nParameters\n{dashes}\nitem\n");
        let (parsed, diagnostics) = parse_entries(&doc);
        assert!(parsed.entries.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC004");
    }

    #[test]
    fn misindented_section_header_is_not_recognized() {
        let doc = "Summary\n Parameters\n----------\nitem2\n\nReturns\n-------\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(parsed.entries.is_empty());
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn misindented_arg_line_yields_no_entries_and_a_diagnostic() {
        let doc = "Summary\n\nParameters\n----------\n bad_indent\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(parsed.entries.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC005");
    }

    #[test]
    fn section_with_no_parameters_section_present_is_empty() {
        let doc = "Summary\nInformation about this class\n\nReturns\n-------\nnothing : None\n    This doesn't return anything, but this description looks like an arg type.\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(parsed.entries.is_empty());
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn full_numpydoc_section_layout_finds_correct_bodies() {
        let doc = "Summary\nParameters\n----------\nHello\n\nAttributes\n----------\nItem\nOther Parameters\n----------------\nmore parameters\n\nRaises\n------\nA Warning\nWarns\n-----\nsends a warning\n\nNotes\n-----\n\nExamples\n--------\nitem";
        let lines = split_lines(doc);
        let margin = compute_margin(&lines, doc);
        let mut diagnostics = Vec::new();
        let sections = find_sections(&lines, doc, margin, &mut diagnostics);
        assert!(diagnostics.is_empty());

        let body_text = |name: &str| -> Option<&str> {
            let idx = SECTIONS.iter().position(|s| *s == name).unwrap();
            sections
                .iter()
                .find(|s| s.canon_index == idx)
                .map(|s| &doc[s.body_start..s.body_end])
        };

        assert_eq!(body_text("Parameters"), Some("Hello\n"));
        assert_eq!(body_text("Attributes"), Some("Item"));
        assert_eq!(body_text("Other Parameters"), Some("more parameters\n"));
        assert_eq!(body_text("Raises"), Some("A Warning"));
        assert_eq!(body_text("Warns"), Some("sends a warning\n"));
        assert_eq!(body_text("Notes"), Some(""));
        assert_eq!(body_text("Examples"), Some("item"));
        assert_eq!(body_text("Methods"), None);
        assert_eq!(body_text("Returns"), None);
    }
}
