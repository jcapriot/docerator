//! Parses `# docerator: <key>[=<value>]` pragma comments and attaches them to the class/def they
//! follow — deliberately placed *between* the `class`/`def` line and the docstring (never above
//! the statement, and never through decorators), so a directive reads as configuring the
//! docstring it sits next to rather than annotating the statement from outside. Two placements
//! are both recognized, and can be combined:
//!
//! - **Trailing**: on the same physical line as the `class`/`def` header's own closing `:`,
//!   e.g. `class Child(Parent):  # docerator: expand_kwargs`. Only one directive fits here (a
//!   physical line has only one trailing comment), so this is the natural spot for a single,
//!   short directive.
//! - **Own line(s)**, stacked directly below the header and directly above the docstring (or
//!   whatever the first real statement in the body is, if there's no docstring) — same
//!   attachment rule as `# noqa`/`# type: ignore` chains, just downward instead of upward: a
//!   blank line or a comment that isn't a `docerator:` directive breaks the chain.
//!
//! When both are present, the trailing directive is resolved first and the stacked block after
//! it wins on any key conflict (it reads last, closest to the docstring it's configuring).
//! Grammar is deliberately one directive per comment line (not comma-joined onto one line) —
//! `override=arg1,arg2` already uses commas for its own value list, so stacking
//! `# docerator: skip` / `# docerator: expand_kwargs=parameters` as separate lines avoids
//! ambiguity about which commas separate directives versus which separate one directive's values.

use std::collections::HashSet;

use ruff_text_size::{TextRange, TextSize};

use crate::style::{Diagnostic, ParamSectionKind, Severity};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Directives {
    pub skip: bool,
    pub style: Option<String>,
    pub overrides: HashSet<String>,
    /// `expand_kwargs` (bare, defaults to `others`) / `expand_kwargs=parameters` /
    /// `expand_kwargs=others` — pull ancestor-documented parameters not literally named in the
    /// local signature into an auto-managed block in the chosen section. Only meaningful when
    /// the entity's signature actually has `**kwargs`; `sync.rs` diagnoses the mismatch
    /// otherwise.
    pub expand_kwargs_into: Option<ParamSectionKind>,
    /// `exclude=arg1,arg2` — names to leave out of an `expand_kwargs` pull. Meaningless without
    /// `expand_kwargs` (named signature parameters are never optional to document).
    pub exclude: HashSet<String>,
}

impl Directives {
    fn apply(&mut self, key: &str, value: Option<&str>, range: TextRange, diagnostics: &mut Vec<Diagnostic>) {
        match key {
            "skip" => {
                if value.is_some() {
                    diagnostics.push(Diagnostic {
                        code: "DOC006",
                        severity: Severity::Warning,
                        message: "'skip' takes no value".to_string(),
                        range,
                    });
                }
                self.skip = true;
            }
            "style" => match value {
                Some(v) if !v.is_empty() => self.style = Some(v.to_string()),
                _ => diagnostics.push(Diagnostic {
                    code: "DOC006",
                    severity: Severity::Warning,
                    message: "'style' requires a value, e.g. style=numpydoc".to_string(),
                    range,
                }),
            },
            "override" => match value {
                Some(v) if !v.is_empty() => {
                    self.overrides
                        .extend(v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string));
                }
                _ => diagnostics.push(Diagnostic {
                    code: "DOC006",
                    severity: Severity::Warning,
                    message: "'override' requires a value, e.g. override=arg1,arg2".to_string(),
                    range,
                }),
            },
            "expand_kwargs" => {
                self.expand_kwargs_into = Some(match value {
                    None => ParamSectionKind::Secondary,
                    Some("parameters") => ParamSectionKind::Primary,
                    Some("others" | "other_parameters") => ParamSectionKind::Secondary,
                    Some(other) => {
                        diagnostics.push(Diagnostic {
                            code: "DOC006",
                            severity: Severity::Warning,
                            message: format!(
                                "'expand_kwargs' value '{other}' not recognized (expected 'parameters' or \
                                 'others', or no value at all) — defaulting to 'others'"
                            ),
                            range,
                        });
                        ParamSectionKind::Secondary
                    }
                });
            }
            "exclude" => match value {
                Some(v) if !v.is_empty() => {
                    self.exclude
                        .extend(v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string));
                }
                _ => diagnostics.push(Diagnostic {
                    code: "DOC006",
                    severity: Severity::Warning,
                    message: "'exclude' requires a value, e.g. exclude=arg1,arg2".to_string(),
                    range,
                }),
            },
            other => diagnostics.push(Diagnostic {
                code: "DOC006",
                severity: Severity::Warning,
                message: format!("unknown docerator directive '{other}'"),
                range,
            }),
        }
    }
}

struct SourceLine {
    start: usize,
    end: usize,
}

fn split_source_lines(source: &str) -> Vec<SourceLine> {
    let bytes = source.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(make_line(source, start, i));
            start = i + 1;
        }
    }
    lines.push(make_line(source, start, bytes.len()));
    lines
}

fn make_line(source: &str, start: usize, mut end: usize) -> SourceLine {
    if end > start && source.as_bytes()[end - 1] == b'\r' {
        end -= 1;
    }
    SourceLine { start, end }
}

const DIRECTIVE_PREFIX: &str = "docerator:";

/// If `line_content` (a full physical line, whitespace and all) is a `# docerator: key[=value]`
/// comment, return its parsed `(key, value)`. Anything else (ordinary comments, code, blanks)
/// returns `None`.
fn try_parse_directive(line_content: &str) -> Option<(String, Option<String>)> {
    let trimmed = line_content.trim_start();
    let after_hash = trimmed.strip_prefix('#')?.trim_start();
    let body = after_hash.strip_prefix(DIRECTIVE_PREFIX)?.trim();
    if body.is_empty() {
        return None;
    }
    match body.split_once('=') {
        Some((key, value)) => Some((key.trim().to_string(), Some(value.trim().to_string()))),
        None => Some((body.to_string(), None)),
    }
}

/// Resolve the directives attached to one `class`/`def` statement, read from *between* its own
/// header and its body's first real statement (the docstring, in the common case) — never above
/// the statement. `header_colon_end` is the byte offset right after the header's own closing
/// `:` (the position a trailing same-line directive would start being searched from);
/// `body_start` is the byte offset of the first real statement in the body (comments never
/// count as AST statements, so this already lands right past any own-line directive comments).
/// When the body is empty (shouldn't happen for a syntactically valid `class`/`def`, but handled
/// defensively), pass `header_colon_end` again for `body_start` — this degrades to only the
/// trailing-same-line check ever finding anything, which is correct: there's no body to hold an
/// own-line comment at all.
pub fn resolve_directives_between(
    source: &str,
    header_colon_end: TextSize,
    body_start: TextSize,
    diagnostics: &mut Vec<Diagnostic>,
) -> Directives {
    let lines = split_source_lines(source);
    resolve_directives_between_with_lines(source, &lines, header_colon_end, body_start, diagnostics)
}

fn resolve_directives_between_with_lines(
    source: &str,
    lines: &[SourceLine],
    header_colon_end: TextSize,
    body_start: TextSize,
    diagnostics: &mut Vec<Diagnostic>,
) -> Directives {
    let header_end = usize::from(header_colon_end);
    let Some(header_line_idx) = lines.iter().position(|l| l.start <= header_end && header_end <= l.end) else {
        return Directives::default();
    };

    // Read top-to-bottom (source) order throughout, so later entries naturally win on key
    // conflicts when applied in sequence below: the trailing same-line directive (if any) reads
    // first, then the own-line stacked block (closer to the docstring) is appended after it.
    let mut collected: Vec<(String, Option<String>, TextRange)> = Vec::new();

    // Trailing: whatever's left on the header's own physical line, after the colon.
    let header_line = &lines[header_line_idx];
    let trailing = &source[header_end..header_line.end];
    if let Some((key, value)) = try_parse_directive(trailing) {
        let range =
            TextRange::new(TextSize::try_from(header_end).unwrap(), TextSize::try_from(header_line.end).unwrap());
        collected.push((key, value, range));
    }

    // Own line(s): a stacked block directly below the header and directly above the body's first
    // real statement, scanning backward from the body -- same "blank line or non-directive
    // comment breaks the chain" rule as the old upward scan, just now bounded below by the
    // header's own line instead of open-ended toward the top of the file.
    let body = usize::from(body_start);
    if let Some(body_idx) = lines.iter().position(|l| l.start <= body && body <= l.end) {
        let mut own_line: Vec<(String, Option<String>, TextRange)> = Vec::new();
        let mut idx = body_idx;
        while idx > header_line_idx + 1 {
            idx -= 1;
            let line = &lines[idx];
            let text = &source[line.start..line.end];
            let trimmed = text.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((key, value)) = try_parse_directive(text) {
                let range =
                    TextRange::new(TextSize::try_from(line.start).unwrap(), TextSize::try_from(line.end).unwrap());
                own_line.push((key, value, range));
                continue;
            }
            break;
        }
        own_line.reverse();
        collected.extend(own_line);
    }

    let mut directives = Directives::default();
    for (key, value, range) in collected {
        directives.apply(&key, value.as_deref(), range, diagnostics);
    }
    directives
}

/// The last `# docerator: style=...` directive found strictly before `before_offset`, ignoring
/// well-formedness of any surrounding code — used for the file-level style default, which by
/// convention sits at the top of the file, before the first class/def.
pub fn file_level_style(source: &str, before_offset: TextSize) -> Option<String> {
    let before = usize::from(before_offset);
    let lines = split_source_lines(source);
    let mut style = None;
    for line in &lines {
        if line.start >= before {
            break;
        }
        let text = &source[line.start..line.end];
        if let Some((key, Some(value))) = try_parse_directive(text) {
            if key == "style" && !value.is_empty() {
                style = Some(value);
            }
        }
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `colon_marker` is a literal substring of `source` ending exactly at the header's own
    /// closing `:` (e.g. `"class Foo:"`); `body_marker` is a literal substring starting exactly
    /// at the body's first real statement (e.g. `"\"\"\"Doc"` or `"pass"`) — mirrors how
    /// `model.rs` computes these two offsets from the AST in real usage, without needing a
    /// parser here.
    fn resolve(source: &str, colon_marker: &str, body_marker: &str) -> (Directives, Vec<Diagnostic>) {
        let header_colon_end = (source.find(colon_marker).unwrap() + colon_marker.len()) as u32;
        let body_start = source.find(body_marker).unwrap() as u32;
        let mut diagnostics = Vec::new();
        let directives = resolve_directives_between(
            source,
            TextSize::from(header_colon_end),
            TextSize::from(body_start),
            &mut diagnostics,
        );
        (directives, diagnostics)
    }

    #[test]
    fn attaches_trailing_same_line_directive() {
        let source = "class Foo:  # docerator: skip\n    \"\"\"Doc.\"\"\"\n";
        let (directives, diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert!(diagnostics.is_empty());
        assert!(directives.skip);
    }

    #[test]
    fn attaches_stacked_own_line_directives_between_header_and_docstring() {
        let source = "class Foo:\n    # docerator: style=numpydoc\n    # docerator: override=arg1,arg2\n    \"\"\"Doc.\"\"\"\n";
        let (directives, diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert!(diagnostics.is_empty());
        assert_eq!(directives.style.as_deref(), Some("numpydoc"));
        assert_eq!(directives.overrides, HashSet::from(["arg1".to_string(), "arg2".to_string()]));
    }

    #[test]
    fn decorators_above_the_def_do_not_affect_resolution() {
        let source = "@staticmethod\n@another_decorator\ndef foo():  # docerator: skip\n    \"\"\"Doc.\"\"\"\n";
        let (directives, diagnostics) = resolve(source, "def foo():", "\"\"\"Doc");
        assert!(diagnostics.is_empty());
        assert!(directives.skip);
    }

    #[test]
    fn directive_placed_above_the_class_is_no_longer_recognized() {
        let source = "# docerator: skip\nclass Foo:\n    \"\"\"Doc.\"\"\"\n";
        let (directives, _diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert!(!directives.skip);
    }

    #[test]
    fn blank_line_between_header_and_own_line_directive_does_not_break_attachment() {
        // The scan is anchored at the docstring and walks backward, so what matters is whether
        // the directive block sits directly above the docstring -- a blank line further up,
        // between the header and the block, is irrelevant (nothing was scanning that far).
        let source = "class Foo:\n\n    # docerator: skip\n    \"\"\"Doc.\"\"\"\n";
        let (directives, _diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert!(directives.skip);
    }

    #[test]
    fn blank_line_between_own_line_directive_and_docstring_breaks_attachment() {
        let source = "class Foo:\n    # docerator: skip\n\n    \"\"\"Doc.\"\"\"\n";
        let (directives, _diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert!(!directives.skip);
    }

    #[test]
    fn unrelated_own_line_comment_breaks_attachment() {
        let source = "class Foo:\n    # docerator: skip\n    # just a normal comment\n    \"\"\"Doc.\"\"\"\n";
        let (directives, _diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert!(!directives.skip);
    }

    #[test]
    fn unknown_key_is_diagnosed_but_entity_still_processed() {
        let source = "class Foo:\n    # docerator: bogus=1\n    \"\"\"Doc.\"\"\"\n";
        let (directives, diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert!(!directives.skip);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC006");
    }

    #[test]
    fn later_stacked_line_wins_on_key_conflict() {
        let source = "class Foo:\n    # docerator: style=google\n    # docerator: style=numpydoc\n    \"\"\"Doc.\"\"\"\n";
        let (directives, _diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert_eq!(directives.style.as_deref(), Some("numpydoc"));
    }

    #[test]
    fn own_line_stacked_directive_wins_over_trailing_on_key_conflict() {
        let source = "class Foo:  # docerator: style=google\n    # docerator: style=numpydoc\n    \"\"\"Doc.\"\"\"\n";
        let (directives, _diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert_eq!(directives.style.as_deref(), Some("numpydoc"));
    }

    #[test]
    fn trailing_and_own_line_directives_combine_when_keys_differ() {
        let source = "class Foo:  # docerator: skip\n    # docerator: style=numpydoc\n    \"\"\"Doc.\"\"\"\n";
        let (directives, diagnostics) = resolve(source, "class Foo:", "\"\"\"Doc");
        assert!(diagnostics.is_empty());
        assert!(directives.skip);
        assert_eq!(directives.style.as_deref(), Some("numpydoc"));
    }

    #[test]
    fn works_when_the_body_has_no_docstring_at_all() {
        let source = "class Foo:\n    # docerator: skip\n    pass\n";
        let (directives, _diagnostics) = resolve(source, "class Foo:", "pass");
        assert!(directives.skip);
    }

    #[test]
    fn file_level_style_found_before_first_class() {
        let source = "# docerator: style=numpydoc\n\nclass Foo:\n    pass\n";
        let class_start = source.find("class").unwrap() as u32;
        assert_eq!(file_level_style(source, TextSize::from(class_start)).as_deref(), Some("numpydoc"));
    }

    #[test]
    fn file_level_style_absent_when_none_declared() {
        let source = "class Foo:\n    pass\n";
        let class_start = source.find("class").unwrap() as u32;
        assert_eq!(file_level_style(source, TextSize::from(class_start)), None);
    }
}
