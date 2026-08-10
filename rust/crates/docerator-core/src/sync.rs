//! The auto-sync engine: for every named signature parameter an ancestor documents, regenerate
//! (or insert) that entry's text every run so it always matches the nearest ancestor that
//! authored it. M2 scope was single-file, same-file base-class resolution with no directives —
//! M3 adds `# docerator:` directive handling: `skip` (exempt an entity, its content still flows
//! to descendants as-is), `override=` (a name is authored here, never auto-managed, regardless
//! of what an ancestor says), and `style=` resolution through entity → file → project → hard
//! default. Still single-file; cross-file resolution is a later milestone.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use ruff_text_size::{TextRange, TextSize};

use crate::edit::{apply_edits, TextEdit};
use crate::model::{self, ClassModel, FileModel, MethodModel};
use crate::parse;
use crate::style::numpydoc::NumpydocStyle;
use crate::style::{Diagnostic, DocStyle, ParamEntry, Severity};

pub fn sync_source(
    source: &str,
    project_default_style: Option<&str>,
) -> Result<(String, Vec<Diagnostic>), ruff_python_parser::ParseError> {
    let parsed = parse::parse(source)?;
    let mut diagnostics = Vec::new();
    let file_model = model::build_file_model(source, parsed.syntax(), &mut diagnostics);

    let mut memo: HashMap<String, MethodViews> = HashMap::new();
    let mut edits = Vec::new();

    let class_names: Vec<String> = file_model.classes.keys().cloned().collect();
    for name in &class_names {
        resolve_and_rewrite(&file_model, name, project_default_style, &mut memo, &mut edits, &mut diagnostics);
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
    project_default_style: Option<&str>,
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
        .map(|base_name| {
            resolve_and_rewrite(file, base_name, project_default_style, memo, edits, diagnostics)
        })
        .unwrap_or_default();

    let mut this_view: MethodViews = IndexMap::new();

    for (method_name, method) in &class.methods {
        let inherited_from_ancestors = parent_view.get(method_name).cloned().unwrap_or_default();
        let effective_skip = class.directives.skip || method.directives.skip;
        let effective_overrides = effective_overrides(class, method_name, method);
        let effective_exclude = effective_exclude(class, method_name, method);
        let effective_expand_target = effective_expand_kwargs_into(class, method_name, method);
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
                let style_name = effective_style_name(class, method_name, method, file.file_level_style.as_deref(), project_default_style);
                let style = resolve_style(&style_name, doc.inner_range, diagnostics);
                let (parsed_entries, parse_diagnostics) = style.parse_entries(&doc.text);

                if effective_skip {
                    // Exempt: no edits, no diagnostics about this entity's own structure — but
                    // its content still flows to descendants exactly as authored elsewhere.
                    for (name, entry) in parsed_entries.iter() {
                        authored.insert(name.clone(), entry.clone());
                    }
                } else {
                    for d in parse_diagnostics {
                        diagnostics.push(offset_diagnostic(d, doc.inner_range.start()));
                    }

                    if effective_expand_target.is_some() && !method.signature.has_var_keyword {
                        diagnostics.push(Diagnostic {
                            code: "DOC009",
                            severity: Severity::Warning,
                            message: "'expand_kwargs' has no effect: this entity's signature has no **kwargs"
                                .to_string(),
                            range: doc.inner_range,
                        });
                    }

                    for name in &method.signature.names {
                        if effective_overrides.contains(name) {
                            continue;
                        }
                        match (parsed_entries.get(name), inherited_from_ancestors.get(name)) {
                            (_, Some(ancestor_entry)) => {
                                let new_text = style.format_entry(ancestor_entry, "");
                                match parsed_entries.get(name) {
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
                                    None => match append_point(&parsed_entries.primary, &doc.text) {
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

                    if let Some(target) = effective_expand_target {
                        if method.signature.has_var_keyword {
                            expand_kwargs(
                                target,
                                &method.signature.names,
                                &effective_overrides,
                                &effective_exclude,
                                &inherited_from_ancestors,
                                &parsed_entries,
                                &doc.text,
                                doc.inner_range.start(),
                                &style,
                                edits,
                            );
                        }
                    }

                    for (name, entry) in parsed_entries.iter() {
                        if effective_overrides.contains(name) || !inherited_from_ancestors.contains_key(name) {
                            authored.insert(name.clone(), entry.clone());
                        }
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

/// `override=` is scoped per entity, with one exception: a class-level directive applies to the
/// `__init__` entity (the class's own docstring, conventionally), since a bare param name
/// attached to the class itself only makes unambiguous sense for the constructor's namespace —
/// it is NOT broadcast to every other method the way `skip` is.
fn effective_overrides(class: &ClassModel, method_name: &str, method: &MethodModel) -> HashSet<String> {
    let mut overrides = method.directives.overrides.clone();
    if method_name == "__init__" {
        overrides.extend(class.directives.overrides.iter().cloned());
    }
    overrides
}

/// `exclude=` follows the same scoping as `override=` — it only makes sense attached to the
/// same entity that carries (or would carry) `expand_kwargs`.
fn effective_exclude(class: &ClassModel, method_name: &str, method: &MethodModel) -> HashSet<String> {
    let mut exclude = method.directives.exclude.clone();
    if method_name == "__init__" {
        exclude.extend(class.directives.exclude.iter().cloned());
    }
    exclude
}

/// The method's own `expand_kwargs` directive wins; otherwise, for the `__init__` entity only,
/// fall back to a class-level one — same scoping as `override=`/`exclude=`.
fn effective_expand_kwargs_into(
    class: &ClassModel,
    method_name: &str,
    method: &MethodModel,
) -> Option<crate::style::ParamSectionKind> {
    method
        .directives
        .expand_kwargs_into
        .or_else(|| if method_name == "__init__" { class.directives.expand_kwargs_into } else { None })
}

/// `style=` resolution order, most specific first: a directive on the method itself; a
/// class-level directive (only honored for the `__init__` entity, same scoping as `override=`);
/// a file-level directive; the project-wide default passed into `sync_source`; the hard fallback.
fn effective_style_name(
    class: &ClassModel,
    method_name: &str,
    method: &MethodModel,
    file_level_style: Option<&str>,
    project_default_style: Option<&str>,
) -> String {
    method
        .directives
        .style
        .clone()
        .or_else(|| {
            if method_name == "__init__" {
                class.directives.style.clone()
            } else {
                None
            }
        })
        .or_else(|| file_level_style.map(str::to_string))
        .or_else(|| project_default_style.map(str::to_string))
        .unwrap_or_else(|| "numpydoc".to_string())
}

/// Only `numpydoc` exists today; an unrecognized name is diagnosed and falls back to it rather
/// than aborting the whole entity — adding a real second style later just means matching more
/// names here (and putting a real registry in front of this once there is more than one).
fn resolve_style(name: &str, range: TextRange, diagnostics: &mut Vec<Diagnostic>) -> NumpydocStyle {
    if name != "numpydoc" {
        diagnostics.push(Diagnostic {
            code: "DOC011",
            severity: Severity::Warning,
            message: format!("unknown docstring style '{name}', falling back to numpydoc"),
            range,
        });
    }
    NumpydocStyle
}

/// Byte offset (relative to a docstring's own interior text) right after the last existing
/// entry in `section_entries`, plus the leading indentation to prepend to a newly-inserted line
/// (a brand-new line has no existing indentation of its own to reuse, unlike a splice-replaced
/// entry) — borrowed from that same last entry's own line, on the assumption every entry in a
/// section shares one indentation level (true for any well-formed numpydoc section). `None`
/// when `section_entries` is empty — there's nothing to anchor to yet in that section.
fn append_point(section_entries: &IndexMap<String, ParamEntry>, docstring_text: &str) -> Option<(usize, String)> {
    let last = section_entries.values().max_by_key(|e| e.range.end())?;
    let insertion = usize::from(last.range.end());
    let line_start = docstring_text[..usize::from(last.range.start())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let indent = docstring_text[line_start..usize::from(last.range.start())].to_string();
    Some((insertion, indent))
}

/// Pulls ancestor-documented parameters that aren't literally named in the local signature into
/// an auto-managed `Other Parameters` block (the `expand=kwargs` directive) — mirrors the named-
/// parameter regenerate-or-insert logic above, but all insertions for entries missing from a
/// not-yet-existing `Other Parameters` section are batched into ONE edit at one anchor point
/// (see the per-call comment below for why: queuing them independently would each try to
/// synthesize their own copy of the section header).
#[allow(clippy::too_many_arguments)]
fn expand_kwargs(
    target: crate::style::ParamSectionKind,
    signature_names: &[String],
    effective_overrides: &HashSet<String>,
    effective_exclude: &HashSet<String>,
    inherited_from_ancestors: &IndexMap<String, ParamEntry>,
    parsed_entries: &crate::style::ParsedEntries,
    docstring_text: &str,
    inner_range_start: TextSize,
    style: &NumpydocStyle,
    edits: &mut Vec<TextEdit>,
) {
    use crate::style::ParamSectionKind;

    let target_entries = match target {
        ParamSectionKind::Primary => &parsed_entries.primary,
        ParamSectionKind::Secondary => &parsed_entries.secondary,
    };

    let named: HashSet<&str> = signature_names.iter().map(String::as_str).collect();
    let mut pending_inserts: Vec<String> = Vec::new();

    for (name, ancestor_entry) in inherited_from_ancestors.iter() {
        if named.contains(name.as_str()) || effective_overrides.contains(name) || effective_exclude.contains(name) {
            continue;
        }
        let new_text = style.format_entry(ancestor_entry, "");
        match target_entries.get(name) {
            Some(existing)
                if existing.type_description == ancestor_entry.type_description
                    && existing.description == ancestor_entry.description =>
            {
                // already in sync
            }
            Some(existing) => {
                edits.push(TextEdit::new(offset_range(existing.range, inner_range_start), new_text));
            }
            None => pending_inserts.push(new_text),
        }
    }

    if pending_inserts.is_empty() {
        return;
    }

    // All queued insertions land at ONE anchor: either the end of the target section if it
    // already exists, or (if it doesn't) right after the *other* section, where a header for
    // the target is synthesized once and prepended to only the first inserted entry — every
    // subsequent entry in the same batch just appends its own self-contained "\n{indent}{text}"
    // block at the identical offset, and `apply_edits`' stable sort by start-offset preserves
    // push order, so they still land in the right sequence.
    //
    // The "anchor after the other section" fallback is only safe for `Secondary` (`Other
    // Parameters` correctly belongs after `Parameters` in canonical numpydoc order); a missing
    // `Parameters` section is never synthesized after an existing `Other Parameters` one, since
    // that would violate section order and make it unparseable on the next run. That combination
    // just has nowhere sensible to go yet.
    let fallback_anchor = match target {
        ParamSectionKind::Primary => None,
        ParamSectionKind::Secondary => append_point(&parsed_entries.primary, docstring_text),
    };
    let (anchor, indent, header) = if let Some((insertion, indent)) = append_point(target_entries, docstring_text) {
        (insertion, indent, String::new())
    } else if let Some((insertion, indent)) = fallback_anchor {
        let raw_header = style.synthesize_section(target);
        let indented_header: String = raw_header.lines().map(|line| format!("{indent}{line}\n")).collect();
        (insertion, indent, format!("\n{indented_header}"))
    } else {
        // No anchor at all -- nothing sensible to append to yet.
        return;
    };

    let at = inner_range_start + TextSize::try_from(anchor).unwrap();
    let mut text = header;
    for (i, entry_text) in pending_inserts.iter().enumerate() {
        if i == 0 && !text.is_empty() {
            // `text` is a freshly-synthesized section header, which already ends in a newline
            // (from its underline's own line) — go straight to the indent, no extra blank line.
            text.push_str(&indent);
        } else {
            text.push('\n');
            text.push_str(&indent);
        }
        text.push_str(entry_text);
    }
    edits.push(TextEdit::new(TextRange::new(at, at), text));
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

    fn sync(source: &str) -> (String, Vec<Diagnostic>) {
        sync_source(source, None).expect("fixture must parse")
    }

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
        let (output, diagnostics) = sync(source);
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
        let (output, diagnostics) = sync(source);
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
        let (output, diagnostics) = sync(source);
        assert_eq!(output, source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC001");
        assert!(diagnostics[0].message.contains("mystery"));
    }

    #[test]
    fn method_level_skip_leaves_stale_text_untouched_and_suppresses_diagnostics() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Fresh description.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Parent):
    # docerator: skip
    def __init__(self, arg1):
        \"\"\"Child init.

        Parameters
        ----------
        arg1 : int
            Deliberately stale, and deliberately left alone.
        \"\"\"
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty());
        assert_eq!(output, source);
    }

    #[test]
    fn class_level_skip_broadcasts_to_every_method() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Fresh.
    \"\"\"

    def __init__(self, arg1):
        pass


# docerator: skip
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale but exempt.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty());
        assert_eq!(output, source);
    }

    #[test]
    fn override_keeps_local_text_and_becomes_the_new_basis_for_descendants() {
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    arg1 : int
        Original description.
    \"\"\"

    def __init__(self, arg1):
        pass


# docerator: override=arg1
class Parent(Grandparent):
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Parent's own, deliberately different, description.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should sync to Parent's override, not Grandparent's original.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert!(output.contains("Parent's own, deliberately different, description."));
        // Child's arg1 must have synced to Parent's override text, not Grandparent's original.
        let child_section = &output[output.find("class Child").unwrap()..];
        assert!(child_section.contains("Parent's own, deliberately different, description."));
        assert!(!child_section.contains("Original description."));
    }

    #[test]
    fn unknown_style_directive_is_diagnosed_but_still_falls_back_to_numpydoc() {
        let source = "\
# docerator: style=google
class Solo:
    \"\"\"Solo.

    Parameters
    ----------
    arg1 : int
        Documented.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert_eq!(output, source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC011");
    }

    #[test]
    fn project_default_style_suppresses_unknown_style_diagnostic_when_valid() {
        let source = "\
class Solo:
    \"\"\"Solo.

    Parameters
    ----------
    arg1 : int
        Documented.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (_output, diagnostics) = sync_source(source, Some("numpydoc")).expect("fixture must parse");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn expand_kwargs_regenerates_existing_other_parameters_entry_and_appends_missing_one() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    extra2 : float
        Extra2 doc.
    \"\"\"

    def __init__(self, arg1, extra1, extra2):
        pass


# docerator: expand_kwargs
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.

    Other Parameters
    ----------------
    extra1 : bool
        Stale, should regenerate.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

        let expected = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    extra2 : float
        Extra2 doc.
    \"\"\"

    def __init__(self, arg1, extra1, extra2):
        pass


# docerator: expand_kwargs
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.

    Other Parameters
    ----------------
    extra1 : bool
        Extra1 doc.
    extra2 : float
        Extra2 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        assert_eq!(output, expected);
    }

    #[test]
    fn expand_kwargs_synthesizes_other_parameters_section_from_scratch_with_no_duplicate_header() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    extra2 : float
        Extra2 doc.
    \"\"\"

    def __init__(self, arg1, extra1, extra2):
        pass


# docerator: expand_kwargs
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

        let expected = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    extra2 : float
        Extra2 doc.
    \"\"\"

    def __init__(self, arg1, extra1, extra2):
        pass


# docerator: expand_kwargs
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    Other Parameters
    ----------------
    extra1 : bool
        Extra1 doc.
    extra2 : float
        Extra2 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        assert_eq!(output, expected);
        // exactly one header, not one per synthesized entry
        assert_eq!(output.matches("Other Parameters").count(), 1);
    }

    #[test]
    fn exclude_suppresses_a_specific_ancestor_parameter_from_expansion() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    extra2 : float
        Extra2 doc.
    \"\"\"

    def __init__(self, arg1, extra1, extra2):
        pass


# docerator: expand_kwargs
# docerator: exclude=extra1
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class Child").unwrap()..];
        assert!(!child_section.contains("extra1"));
        assert!(child_section.contains("extra2"));
    }

    #[test]
    fn expand_kwargs_without_var_keyword_is_diagnosed() {
        let source = "\
# docerator: expand_kwargs
class Solo:
    \"\"\"Solo.

    Parameters
    ----------
    arg1 : int
        Documented.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert_eq!(output, source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC009");
    }

    #[test]
    fn expand_kwargs_into_parameters_inserts_alongside_named_params_not_into_other_parameters() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    \"\"\"

    def __init__(self, arg1, extra1):
        pass


# docerator: expand_kwargs=parameters
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

        let expected = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    \"\"\"

    def __init__(self, arg1, extra1):
        pass


# docerator: expand_kwargs=parameters
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        assert_eq!(output, expected);
        assert!(!output.contains("Other Parameters"));
    }

    #[test]
    fn expand_kwargs_unrecognized_value_is_diagnosed_and_defaults_to_others() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc.
    \"\"\"

    def __init__(self, arg1, extra1):
        pass


# docerator: expand_kwargs=bogus
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC006");
        assert!(output.contains("Other Parameters"));
        assert!(output.contains("extra1"));
    }
}
