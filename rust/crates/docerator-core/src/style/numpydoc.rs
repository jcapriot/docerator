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

    fn format_entry(&self, entry: &ParamEntry, _indent: &str, newline: &str) -> String {
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
            out.push_str(newline);
            out.push_str(desc);
        }
        out
    }

    fn synthesize_section(&self, section: ParamSectionKind) -> String {
        let name = match section {
            ParamSectionKind::Primary => "Parameters",
            ParamSectionKind::Secondary => "Other Parameters",
        };
        format!("{name}\n{}\n", "-".repeat(name.chars().count()))
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

/// The start of the physical line right after the one ending at `line_end`. `Line::end` always
/// excludes a trailing `\r` from its own content (so a CRLF-terminated line's indent/content
/// computations stay clean) — which means `line_end` sits *on* the `\r` for a CRLF line, one
/// byte short of `line_end + 1` actually reaching the next line's real start. Scanning forward
/// for the actual `\n` byte and stepping past it is correct for both `\n`- and `\r\n`-terminated
/// input without needing to track which one applied. Getting this wrong doesn't move any entry's
/// byte *range* (its end is independently derived and self-corrects via `trim_end`), but it does
/// corrupt the captured description *text* — the line's own `\n` reappears as a spurious leading
/// blank line, which then gets faithfully reproduced (per the "preserve authored formatting"
/// rule) everywhere that description is auto-synced.
fn next_line_start(source: &str, line_end: usize) -> usize {
    match source.as_bytes()[line_end..].iter().position(|&b| b == b'\n') {
        Some(offset) => line_end + offset + 1,
        None => line_end,
    }
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
    /// Byte offset of the start of this section's own header line (`<Name>`, not its
    /// underline) — used to anchor an insertion *before* this section when synthesizing a
    /// section that canonically belongs earlier (e.g. inserting a missing `Parameters` section
    /// ahead of an existing `Returns` one).
    header_start: usize,
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
            header_start: lines[header_idx].start,
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

/// Parse one section's own body in isolation — every boundary (the "next arg line", the
/// fallback end-of-description) stays within this section's own `body_end`, so `Parameters`
/// and `Other Parameters` can always be parsed independently with no cross-section bleed.
fn parse_section(lines: &[Line], source: &str, section: &SectionBody, margin: usize) -> IndexMap<String, ParamEntry> {
    let mut arg_line_indices: Vec<usize> = Vec::new();
    collect_arg_lines(lines, source, section.body_start, section.body_end, margin, &mut arg_line_indices);

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

        let desc_start = next_line_start(source, line.end);
        let raw_desc_end = match arg_line_indices.get(pos + 1) {
            Some(&next_idx) => lines[next_idx].start.saturating_sub(1).min(section.body_end),
            None => section.body_end,
        };

        // Trim only trailing whitespace, never leading: an author-written blank line between
        // the `name : type` line and the description is part of that entry's own authored
        // formatting and must be preserved verbatim so it's reproduced faithfully wherever this
        // entry gets auto-synced — it is NOT the tool's place to normalize an author's spacing
        // choice within their own content. Trailing whitespace is different: it's never really
        // "this entry's content" at all, it's the gap before whatever comes next (another
        // parameter, or — for the last entry in a section immediately followed by another
        // section — the next section's header, picked up as one incidental trailing newline by
        // the same slicing rule that correctly excludes the header text itself). Stopping the
        // range at the end of the last real content line, however many blank lines follow,
        // means splicing never reaches into that trailing gap at all — so whatever spacing
        // already exists at the splice *target* (zero blank lines or several) is left
        // completely untouched, never stripped and never padded, matching "keep what's there,
        // insert nothing of your own." Interior blank lines (a genuine multi-paragraph
        // description) are untouched either way since trimming only ever shortens from one end.
        let (description, desc_end) = if desc_start < raw_desc_end {
            let trimmed = source[desc_start..raw_desc_end].trim_end();
            if trimmed.is_empty() {
                (None, line.end)
            } else {
                (Some(trimmed.to_string()), desc_start + trimmed.len())
            }
        } else {
            (None, line.end)
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
                    // Stamped in by the caller via `ParsedEntries::set_is_raw` once the source
                    // docstring's raw-ness is known -- parsing itself is style-agnostic.
                    is_raw: false,
                },
            );
        }
    }
    entries
}

fn parse_entries(source: &str) -> (ParsedEntries, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let lines = split_lines(source);
    let margin = compute_margin(&lines, source);
    let sections = find_sections(&lines, source, margin, &mut diagnostics);

    let params_section = sections.iter().find(|s| s.canon_index == PARAMETERS_INDEX);
    let others_section = sections.iter().find(|s| s.canon_index == OTHER_PARAMETERS_INDEX);

    let primary = params_section
        .map(|s| parse_section(&lines, source, s, margin))
        .unwrap_or_default();
    let secondary = others_section
        .map(|s| parse_section(&lines, source, s, margin))
        .unwrap_or_default();

    if let Some(s) = params_section {
        if primary.is_empty() {
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

    (
        ParsedEntries {
            primary,
            secondary,
            has_primary_section: params_section.is_some(),
            // `sections` is built by `find_sections` scanning top-to-bottom, and a section is
            // only ever accepted (pushed) when its canon_index exceeds every previously accepted
            // one — an out-of-order header is rejected (DOC003), never accepted out of position
            // — so acceptance order and text order coincide: the first element, if any, really
            // is the textually-first recognized section, regardless of its own kind.
            first_section_start: sections.first().map(|s| s.header_start),
            margin_indent: " ".repeat(margin),
        },
        diagnostics,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn entry_names(parsed: &ParsedEntries) -> Vec<&str> {
        parsed.iter().map(|(name, _)| name.as_str()).collect()
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

        let get = |n: &str| parsed.get(n).unwrap();
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
            Some("    I've got a description line\n    that ends with an empty line.")
        );
        assert_eq!(get("multiple").type_description.as_deref(), Some("shared type"));
        assert_eq!(get("multiple").description.as_deref(), Some("    Shared Description"));
        assert_eq!(get("args").type_description.as_deref(), Some("shared type"));
        assert_eq!(get("args").description.as_deref(), Some("    Shared Description"));
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
        assert!(parsed.is_empty());
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
        assert!(parsed.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC004");
    }

    #[test]
    fn misindented_section_header_is_not_recognized() {
        let doc = "Summary\n Parameters\n----------\nitem2\n\nReturns\n-------\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(parsed.is_empty());
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn misindented_arg_line_yields_no_entries_and_a_diagnostic() {
        let doc = "Summary\n\nParameters\n----------\n bad_indent\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(parsed.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC005");
    }

    #[test]
    fn leading_blank_line_before_description_is_preserved_verbatim() {
        let doc = "Summary\n\nParameters\n----------\narg1 : int\n\n    Description with a blank line above it.\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(diagnostics.is_empty());
        assert_eq!(
            parsed.get("arg1").unwrap().description.as_deref(),
            Some("\n    Description with a blank line above it.")
        );
    }

    #[test]
    fn multiple_leading_blank_lines_before_description_are_all_preserved() {
        let doc = "Summary\n\nParameters\n----------\narg1 : int\n\n\n    Description after two blank lines.\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(diagnostics.is_empty());
        assert_eq!(
            parsed.get("arg1").unwrap().description.as_deref(),
            Some("\n\n    Description after two blank lines.")
        );
    }

    #[test]
    fn arg_with_no_description_at_all_does_not_panic() {
        let doc = "Summary\n\nParameters\n----------\narg1 : int\narg2 : str\n    Has a description.\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(diagnostics.is_empty());
        assert_eq!(parsed.get("arg1").unwrap().description, None);
        assert_eq!(parsed.get("arg2").unwrap().description.as_deref(), Some("    Has a description."));
    }

    #[test]
    fn crlf_line_endings_do_not_produce_a_spurious_leading_blank_line() {
        // Regression test: `desc_start` used to be computed as `line.end + 1`, which is only
        // correct for `\n`-terminated input. For `\r\n` lines, `Line::end` sits *on* the `\r`
        // (it's excluded from the line's own content), so `+ 1` landed on the `\n` itself
        // instead of past it -- the arg-name line's own newline then reappeared as a spurious
        // leading `\n` in the captured description, silently invisible on disk (nothing ever
        // spliced it) until the entry was auto-synced somewhere else, at which point the
        // erroneous leading newline became a real, visible blank line in the regenerated text.
        let lf = "Summary\n\nParameters\n----------\narg1 : int\n    Clean description.\n";
        let crlf = lf.replace('\n', "\r\n");
        let (parsed, diagnostics) = parse_entries(&crlf);
        assert!(diagnostics.is_empty());
        assert_eq!(parsed.get("arg1").unwrap().description.as_deref(), Some("    Clean description."));
    }

    #[test]
    fn section_with_no_parameters_section_present_is_empty() {
        let doc = "Summary\nInformation about this class\n\nReturns\n-------\nnothing : None\n    This doesn't return anything, but this description looks like an arg type.\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert!(parsed.is_empty());
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

    #[test]
    fn no_sections_at_all_reports_no_primary_section_and_no_first_section_start() {
        let doc = "Just a summary line, no sections at all.\n";
        let (parsed, _diagnostics) = parse_entries(doc);
        assert!(!parsed.has_primary_section);
        assert_eq!(parsed.first_section_start, None);
    }

    #[test]
    fn first_section_start_points_at_the_textually_first_section_regardless_of_kind() {
        let doc = "Summary\n\nReturns\n-------\nnothing : None\n    Nothing.\n";
        let (parsed, _diagnostics) = parse_entries(doc);
        assert!(!parsed.has_primary_section);
        let expected = doc.find("Returns").unwrap();
        assert_eq!(parsed.first_section_start, Some(expected));
    }

    #[test]
    fn has_primary_section_is_true_even_when_it_parsed_no_entries() {
        // A present-but-empty Parameters header (DOC005) must be reported as "has a section" --
        // never as "missing" -- so a caller deciding whether to synthesize one from scratch
        // never ends up creating a duplicate header right on top of the malformed one.
        let doc = "Summary\n\nParameters\n----------\n bad_indent\n";
        let (parsed, diagnostics) = parse_entries(doc);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC005");
        assert!(parsed.has_primary_section);
    }

    #[test]
    fn margin_indent_matches_the_shared_indentation_of_arg_and_section_lines() {
        let doc = "Summary\n\n    Parameters\n    ----------\n    arg1 : int\n        Doc.\n";
        let (parsed, _diagnostics) = parse_entries(doc);
        assert_eq!(parsed.margin_indent, "    ");
    }
}
