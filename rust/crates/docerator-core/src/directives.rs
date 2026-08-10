//! Parses `# docerator: <key>[=<value>]` pragma comments and attaches them to the class/def
//! they precede — the same convention as `# noqa`/`# type: ignore`: a comment binds through any
//! decorators to the statement it decorates; a blank line or a comment that isn't a `docerator:`
//! directive breaks the chain. Grammar is deliberately one directive per comment line (not
//! comma-joined onto one line) — `override=arg1,arg2` already uses commas for its own value
//! list, so stacking `# docerator: skip` / `# docerator: expand=kwargs` as separate lines avoids
//! ambiguity about which commas separate directives versus which separate one directive's values.

use std::collections::HashSet;

use ruff_text_size::{TextRange, TextSize};

use crate::style::{Diagnostic, Severity};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Directives {
    pub skip: bool,
    pub style: Option<String>,
    pub overrides: HashSet<String>,
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

/// Walk upward from `anchor_start` (the topmost decorator's start if the entity has decorators,
/// otherwise the `class`/`def` statement's own start) collecting directive comments, stopping at
/// the first blank line or non-directive line. Returns the resolved, merged directive set —
/// later (i.e. closer-to-the-entity) lines win on key conflicts, matching top-to-bottom reading
/// order once the collected lines are reversed back into source order.
pub fn resolve_directives_for(source: &str, anchor_start: TextSize, diagnostics: &mut Vec<Diagnostic>) -> Directives {
    let lines = split_source_lines(source);
    resolve_directives_with_lines(source, &lines, anchor_start, diagnostics)
}

fn resolve_directives_with_lines(
    source: &str,
    lines: &[SourceLine],
    anchor_start: TextSize,
    diagnostics: &mut Vec<Diagnostic>,
) -> Directives {
    let anchor = usize::from(anchor_start);
    let Some(anchor_idx) = lines.iter().position(|l| l.start <= anchor && anchor <= l.end) else {
        return Directives::default();
    };

    let mut collected: Vec<(String, Option<String>, TextRange)> = Vec::new();
    let mut idx = anchor_idx;
    while idx > 0 {
        idx -= 1;
        let line = &lines[idx];
        let text = &source[line.start..line.end];
        let trimmed = text.trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.starts_with('@') {
            continue;
        }
        if let Some((key, value)) = try_parse_directive(text) {
            let range = TextRange::new(TextSize::try_from(line.start).unwrap(), TextSize::try_from(line.end).unwrap());
            collected.push((key, value, range));
            continue;
        }
        break;
    }
    collected.reverse();

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

    fn diagnostics_for(source: &str, anchor_start: u32) -> (Directives, Vec<Diagnostic>) {
        let mut diagnostics = Vec::new();
        let directives = resolve_directives_for(source, TextSize::from(anchor_start), &mut diagnostics);
        (directives, diagnostics)
    }

    #[test]
    fn attaches_single_directive_directly_above() {
        let source = "# docerator: skip\nclass Foo:\n    pass\n";
        let anchor = source.find("class").unwrap() as u32;
        let (directives, diagnostics) = diagnostics_for(source, anchor);
        assert!(diagnostics.is_empty());
        assert!(directives.skip);
    }

    #[test]
    fn stacked_lines_merge() {
        let source = "# docerator: style=numpydoc\n# docerator: override=arg1,arg2\nclass Foo:\n    pass\n";
        let anchor = source.find("class").unwrap() as u32;
        let (directives, diagnostics) = diagnostics_for(source, anchor);
        assert!(diagnostics.is_empty());
        assert_eq!(directives.style.as_deref(), Some("numpydoc"));
        assert_eq!(directives.overrides, HashSet::from(["arg1".to_string(), "arg2".to_string()]));
    }

    #[test]
    fn attaches_through_decorators() {
        let source = "# docerator: skip\n@staticmethod\n@another_decorator\ndef foo():\n    pass\n";
        let anchor = source.find("@staticmethod").unwrap() as u32;
        let (directives, diagnostics) = diagnostics_for(source, anchor);
        assert!(diagnostics.is_empty());
        assert!(directives.skip);
    }

    #[test]
    fn blank_line_breaks_attachment() {
        let source = "# docerator: skip\n\nclass Foo:\n    pass\n";
        let anchor = source.find("class").unwrap() as u32;
        let (directives, _diagnostics) = diagnostics_for(source, anchor);
        assert!(!directives.skip);
    }

    #[test]
    fn unrelated_comment_breaks_attachment() {
        let source = "# docerator: skip\n# just a normal comment\nclass Foo:\n    pass\n";
        let anchor = source.find("class").unwrap() as u32;
        let (directives, _diagnostics) = diagnostics_for(source, anchor);
        assert!(!directives.skip);
    }

    #[test]
    fn trailing_same_line_comment_does_not_attach() {
        // resolve_directives_for is only ever called with the statement's own start, so a
        // same-line trailing comment (which lives on the SAME line, after the anchor) is never
        // walked at all -- this test documents that expectation rather than exercising new code.
        let source = "class Foo:  # docerator: skip\n    pass\n";
        let anchor = source.find("class").unwrap() as u32;
        let (directives, _diagnostics) = diagnostics_for(source, anchor);
        assert!(!directives.skip);
    }

    #[test]
    fn unknown_key_is_diagnosed_but_entity_still_processed() {
        let source = "# docerator: bogus=1\nclass Foo:\n    pass\n";
        let anchor = source.find("class").unwrap() as u32;
        let (directives, diagnostics) = diagnostics_for(source, anchor);
        assert!(!directives.skip);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC006");
    }

    #[test]
    fn later_stacked_line_wins_on_key_conflict() {
        let source = "# docerator: style=google\n# docerator: style=numpydoc\nclass Foo:\n    pass\n";
        let anchor = source.find("class").unwrap() as u32;
        let (directives, _diagnostics) = diagnostics_for(source, anchor);
        assert_eq!(directives.style.as_deref(), Some("numpydoc"));
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
