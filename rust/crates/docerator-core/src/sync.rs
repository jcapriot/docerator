//! The auto-sync engine: for every named signature parameter an ancestor documents, regenerate
//! (or insert) that entry's text every run so it always matches the nearest ancestor that
//! authored it. M2 scope: single file, same-file base-class resolution by name, no
//! `# docerator:` directives yet — every inherited, name-matched parameter is unconditionally
//! auto-managed (the future `override=` directive is what will let a class opt a parameter out).

use std::collections::HashMap;

use indexmap::IndexMap;
use ruff_text_size::{TextRange, TextSize};

use crate::edit::{apply_edits, TextEdit};
use crate::model::{self, FileModel};
use crate::parse;
use crate::style::numpydoc::NumpydocStyle;
use crate::style::{Diagnostic, DocStyle, ParamEntry, Severity};

pub fn sync_source(source: &str) -> Result<(String, Vec<Diagnostic>), ruff_python_parser::ParseError> {
    let parsed = parse::parse(source)?;
    let file_model = model::build_file_model(source, parsed.syntax());

    let mut memo: HashMap<String, MethodViews> = HashMap::new();
    let mut edits = Vec::new();
    let mut diagnostics = Vec::new();

    let class_names: Vec<String> = file_model.classes.keys().cloned().collect();
    for name in &class_names {
        resolve_and_rewrite(&file_model, name, &mut memo, &mut edits, &mut diagnostics);
    }

    let output = apply_edits(source, edits);
    Ok((output, diagnostics))
}

/// Per method name, the full flattened "most-derived-that-defines-it wins" view of every entry
/// authored anywhere in the ancestor chain up to and including this class — what a descendant
/// sees when it looks up this class as an ancestor.
type MethodViews = IndexMap<String, IndexMap<String, ParamEntry>>;

fn resolve_and_rewrite(
    file: &FileModel,
    class_name: &str,
    memo: &mut HashMap<String, MethodViews>,
    edits: &mut Vec<TextEdit>,
    diagnostics: &mut Vec<Diagnostic>,
) -> MethodViews {
    if let Some(cached) = memo.get(class_name) {
        return cached.clone();
    }

    let class = &file.classes[class_name];
    let parent_view: MethodViews = class
        .base_names
        .iter()
        .find(|base| file.classes.contains_key(base.as_str()))
        .map(|base_name| resolve_and_rewrite(file, base_name, memo, edits, diagnostics))
        .unwrap_or_default();

    let style = NumpydocStyle;
    let mut this_view: MethodViews = IndexMap::new();

    for (method_name, method) in &class.methods {
        let inherited_from_ancestors = parent_view.get(method_name).cloned().unwrap_or_default();
        let mut authored: IndexMap<String, ParamEntry> = IndexMap::new();

        if let Some(doc) = &method.docstring {
            if doc.text.contains('\\') {
                diagnostics.push(Diagnostic {
                    code: "DOC008",
                    severity: Severity::Error,
                    message: "docstring contains a backslash escape; rewriting is unsupported \
                              for this entity in v1"
                        .to_string(),
                    range: doc.inner_range,
                });
            } else {
                let (parsed_entries, parse_diagnostics) = style.parse_entries(&doc.text);
                for d in parse_diagnostics {
                    diagnostics.push(offset_diagnostic(d, doc.inner_range.start()));
                }

                for name in &method.signature.names {
                    match (parsed_entries.entries.get(name), inherited_from_ancestors.get(name)) {
                        (_, Some(ancestor_entry)) => {
                            let new_text = style.format_entry(ancestor_entry, "");
                            match parsed_entries.entries.get(name) {
                                Some(existing)
                                    if existing.type_description == ancestor_entry.type_description
                                        && existing.description == ancestor_entry.description =>
                                {
                                    // already in sync, nothing to splice
                                }
                                Some(existing) => {
                                    edits.push(TextEdit::new(
                                        offset_range(existing.range, doc.inner_range.start()),
                                        new_text,
                                    ));
                                }
                                None => match append_point(&parsed_entries, &doc.text) {
                                    Some((insertion, indent)) => {
                                        let at = doc.inner_range.start() + TextSize::try_from(insertion).unwrap();
                                        edits.push(TextEdit::new(
                                            TextRange::new(at, at),
                                            format!("\n{indent}{new_text}"),
                                        ));
                                    }
                                    None => {
                                        diagnostics.push(Diagnostic {
                                            code: "DOC010",
                                            severity: Severity::Warning,
                                            message: format!(
                                                "parameter '{name}' is inherited but this docstring has no \
                                                 Parameters section to insert it into yet"
                                            ),
                                            range: doc.inner_range,
                                        });
                                    }
                                },
                            }
                        }
                        (None, None) => {
                            diagnostics.push(Diagnostic {
                                code: "DOC001",
                                severity: Severity::Warning,
                                message: format!(
                                    "parameter '{name}' is not documented locally and no ancestor documents it"
                                ),
                                range: doc.inner_range,
                            });
                        }
                        (Some(_), None) => {}
                    }
                }

                for (name, entry) in parsed_entries.entries.iter() {
                    if !inherited_from_ancestors.contains_key(name) {
                        authored.insert(name.clone(), entry.clone());
                    }
                }
            }
        }

        let mut resolved = inherited_from_ancestors;
        for (name, entry) in authored {
            resolved.insert(name, entry);
        }
        this_view.insert(method_name.clone(), resolved);
    }

    memo.insert(class_name.to_string(), this_view.clone());
    this_view
}

/// Byte offset (relative to a docstring's own interior text) right after the last existing
/// entry, plus the leading indentation to prepend to a newly-inserted line (a brand-new line
/// has no existing indentation of its own to reuse, unlike a splice-replaced entry) — borrowed
/// from that same last entry's own line, on the assumption every entry in a section shares one
/// indentation level (true for any well-formed numpydoc section). `None` when there's nothing
/// to anchor to yet (an empty/missing Parameters section); creating a section from scratch is
/// deferred to a later milestone.
fn append_point(parsed: &crate::style::ParsedEntries, docstring_text: &str) -> Option<(usize, String)> {
    let last = parsed.entries.values().max_by_key(|e| e.range.end())?;
    let insertion = usize::from(last.range.end());
    let line_start = docstring_text[..usize::from(last.range.start())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let indent = docstring_text[line_start..usize::from(last.range.start())].to_string();
    Some((insertion, indent))
}

fn offset_range(range: TextRange, base: TextSize) -> TextRange {
    TextRange::new(base + range.start(), base + range.end())
}

fn offset_diagnostic(diagnostic: Diagnostic, base: TextSize) -> Diagnostic {
    Diagnostic {
        range: offset_range(diagnostic.range, base),
        ..diagnostic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regenerates_stale_inherited_entry_and_inserts_missing_one_leaving_local_param_untouched() {
        let source = "\
class Parent:
    \"\"\"A parent class.

    Parameters
    ----------
    arg1 : int
        The first argument, straight from Parent.
    arg2 : str
        The second argument, also from Parent.
    \"\"\"

    def __init__(self, arg1, arg2):
        pass


class Child(Parent):
    \"\"\"A child class.

    Parameters
    ----------
    arg1 : int
        This text is stale and should be regenerated to match Parent.
    extra : bool
        This one is genuinely new to Child and must be left alone.
    \"\"\"

    def __init__(self, arg1, arg2, extra):
        pass
";
        let (output, diagnostics) = sync_source(source).expect("fixture must parse");
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

        let expected = "\
class Parent:
    \"\"\"A parent class.

    Parameters
    ----------
    arg1 : int
        The first argument, straight from Parent.
    arg2 : str
        The second argument, also from Parent.
    \"\"\"

    def __init__(self, arg1, arg2):
        pass


class Child(Parent):
    \"\"\"A child class.

    Parameters
    ----------
    arg1 : int
        The first argument, straight from Parent.
    extra : bool
        This one is genuinely new to Child and must be left alone.
    arg2 : str
        The second argument, also from Parent.
    \"\"\"

    def __init__(self, arg1, arg2, extra):
        pass
";
        assert_eq!(output, expected);
    }

    #[test]
    fn already_in_sync_produces_no_edits() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Description.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Description.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync_source(source).expect("fixture must parse");
        assert!(diagnostics.is_empty());
        assert_eq!(output, source);
    }

    #[test]
    fn undocumented_and_uninherited_parameter_is_diagnosed() {
        let source = "\
class Lonely:
    \"\"\"Lonely.

    Parameters
    ----------
    documented : int
        Has docs.
    \"\"\"

    def __init__(self, documented, mystery):
        pass
";
        let (output, diagnostics) = sync_source(source).expect("fixture must parse");
        assert_eq!(output, source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC001");
        assert!(diagnostics[0].message.contains("mystery"));
    }
}
