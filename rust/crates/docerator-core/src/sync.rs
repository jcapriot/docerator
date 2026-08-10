//! The auto-sync engine: for every named signature parameter an ancestor documents, regenerate
//! (or insert) that entry's text every run so it always matches the nearest ancestor that
//! authored it. M3 added `# docerator:` directives (`skip`, `override=`, `style=`); M4 added
//! `expand_kwargs`/`exclude=`; M5 makes ancestor resolution project-wide instead of same-file-
//! only: a base class reached through a relative import, an absolute import (src-layout aware —
//! see `project.rs`), or a name re-exported through a package's `__init__.py` all resolve the
//! same way a same-file base class always did. `sync_source` (single file) is now a thin
//! wrapper around `sync_project` (the one real implementation) with a synthetic one-file project.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use indexmap::IndexMap;
use ruff_text_size::{TextRange, TextSize};

use crate::cache::{self, Cache};
use crate::edit::{apply_edits, TextEdit};
use crate::model::{self, ClassModel, MethodModel};
use crate::parse;
use crate::project::{self, ClassId, ProjectFile, ProjectModel};
use crate::style::numpydoc::NumpydocStyle;
use crate::style::{Diagnostic, DocStyle, ParamEntry, Severity};

pub struct FileOutput {
    pub path: PathBuf,
    pub text: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// Sync every class across every file in `files` against the whole project's inheritance graph.
/// `files` should already be `.py` files only — filtering out anything else (and walking a real
/// directory tree in the first place) is the CLI's job, not this library's.
pub fn sync_project(files: &[(PathBuf, String)], project_default_style: Option<&str>) -> Vec<FileOutput> {
    sync_project_inner(files, project_default_style, None).0
}

/// Same as `sync_project`, but consults `cache` for files whose content — and whose entire
/// transitive ancestor chain's content — are unchanged since the run that produced it, skipping
/// the numpydoc-parsing/MRO-merge/diagnostic-generation work for those files entirely (see
/// `cache.rs` for exactly what is and isn't skipped, and why). `cache` is replaced with the
/// updated cache reflecting this run — the caller is responsible for persisting it.
pub fn sync_project_with_cache(files: &[(PathBuf, String)], project_default_style: Option<&str>, cache: &mut Cache) -> Vec<FileOutput> {
    let (outputs, new_cache) = sync_project_inner(files, project_default_style, Some(&*cache));
    *cache = new_cache;
    outputs
}

fn sync_project_inner(
    files: &[(PathBuf, String)],
    project_default_style: Option<&str>,
    old_cache: Option<&Cache>,
) -> (Vec<FileOutput>, Cache) {
    let mut diagnostics_by_file: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
    let project = project::build_project(files, &mut diagnostics_by_file);
    let file_by_path: HashMap<PathBuf, &ProjectFile> = project.files.iter().map(|f| (f.path.clone(), f)).collect();

    let content_hash: HashMap<PathBuf, String> = files.iter().map(|(p, t)| (p.clone(), cache::hash_text(t))).collect();
    let module_hash: HashMap<String, String> = project
        .files
        .iter()
        .filter_map(|f| content_hash.get(&f.path).map(|h| (f.module_name.clone(), h.clone())))
        .collect();
    let module_names: Vec<String> = project.files.iter().map(|f| f.module_name.clone()).collect();
    let global_key = cache::compute_global_key(project_default_style, &module_names);

    let usable_old_cache = old_cache.filter(|c| !c.is_stale() && c.global_key == global_key);

    let mut memo: HashMap<ClassId, MethodViews> = HashMap::new();
    let mut cache_valid_files: HashSet<PathBuf> = HashSet::new();

    if let Some(old) = usable_old_cache {
        for file in &project.files {
            let Some(hash) = content_hash.get(&file.path) else { continue };
            let Some(cached_file) = old.files.get(&file.path) else { continue };
            if &cached_file.content_hash == hash && cache::dependencies_still_valid(&cached_file.dependencies, &module_hash) {
                cache_valid_files.insert(file.path.clone());
                for (class_name, cached_views) in &cached_file.classes {
                    let id = ClassId {
                        module: file.module_name.clone(),
                        name: class_name.clone(),
                    };
                    memo.insert(id, cache::cached_to_method_views(cached_views));
                }
            }
        }
    }

    let mut edits_by_file: HashMap<PathBuf, Vec<TextEdit>> = HashMap::new();
    let class_ids: Vec<ClassId> = project
        .files
        .iter()
        .flat_map(|f| {
            f.model
                .classes
                .keys()
                .map(move |name| ClassId {
                    module: f.module_name.clone(),
                    name: name.clone(),
                })
        })
        .collect();
    for id in &class_ids {
        resolve_and_rewrite(&project, id, project_default_style, &mut memo, &mut edits_by_file, &mut diagnostics_by_file);
    }

    let mut new_cache_files: HashMap<PathBuf, cache::CachedFile> = HashMap::new();
    let mut outputs = Vec::with_capacity(files.len());

    for (path, text) in files {
        if cache_valid_files.contains(path) {
            if let Some(cached) = usable_old_cache.and_then(|old| old.files.get(path)) {
                outputs.push(FileOutput {
                    path: path.clone(),
                    text: cached.output_text.clone(),
                    diagnostics: cached.diagnostics.iter().map(cache::cached_to_diagnostic).collect(),
                });
                new_cache_files.insert(path.clone(), cached.clone());
                continue;
            }
        }

        let edits = edits_by_file.remove(path).unwrap_or_default();
        let diagnostics = diagnostics_by_file.remove(path).unwrap_or_default();
        let output_text = apply_edits(text, edits);

        if let Some(&file) = file_by_path.get(path) {
            let dependencies: Vec<(String, String)> = project::transitive_dependency_files(&project, file)
                .iter()
                .filter_map(|dep_path| {
                    let dep_file = file_by_path.get(dep_path)?;
                    let hash = content_hash.get(dep_path)?;
                    Some((dep_file.module_name.clone(), hash.clone()))
                })
                .collect();

            let classes: HashMap<String, cache::CachedMethodViews> = file
                .model
                .classes
                .keys()
                .map(|name| {
                    let id = ClassId {
                        module: file.module_name.clone(),
                        name: name.clone(),
                    };
                    let views = memo.get(&id).cloned().unwrap_or_default();
                    (name.clone(), cache::method_views_to_cached(&views))
                })
                .collect();

            new_cache_files.insert(
                path.clone(),
                cache::CachedFile {
                    content_hash: content_hash.get(path).cloned().unwrap_or_default(),
                    dependencies,
                    output_text: output_text.clone(),
                    diagnostics: diagnostics.iter().map(cache::diagnostic_to_cached).collect(),
                    classes,
                },
            );
        }

        outputs.push(FileOutput {
            path: path.clone(),
            text: output_text,
            diagnostics,
        });
    }

    let new_cache = Cache {
        format_version: cache::CACHE_FORMAT_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        global_key,
        files: new_cache_files,
    };

    (outputs, new_cache)
}

/// Single-file convenience wrapper over `sync_project`, kept for the common case (and every
/// existing single-file test) — a lone file behaves exactly as it always did, since an
/// unresolvable/absent import for a base class falls back to the same same-file lookup this
/// used before M5.
pub fn sync_source(
    source: &str,
    project_default_style: Option<&str>,
) -> Result<(String, Vec<Diagnostic>), ruff_python_parser::ParseError> {
    parse::parse(source)?;
    let path = PathBuf::from("<source>.py");
    let files = [(path.clone(), source.to_string())];
    let mut outputs = sync_project(&files, project_default_style);
    let output = outputs.pop().expect("sync_project returns one output per input file");
    Ok((output.text, output.diagnostics))
}

/// Per method name, the full flattened "most-derived-that-defines-it wins" view of every entry
/// authored anywhere in the ancestor chain up to and including this class — what a descendant
/// sees when it looks up this class as an ancestor.
type MethodViews = IndexMap<String, IndexMap<String, ParamEntry>>;

fn resolve_and_rewrite(
    project: &ProjectModel,
    class_id: &ClassId,
    project_default_style: Option<&str>,
    memo: &mut HashMap<ClassId, MethodViews>,
    edits_by_file: &mut HashMap<PathBuf, Vec<TextEdit>>,
    diagnostics_by_file: &mut HashMap<PathBuf, Vec<Diagnostic>>,
) -> MethodViews {
    if let Some(cached) = memo.get(class_id) {
        return cached.clone();
    }

    let Some((file, class)) = project.class(class_id) else {
        return IndexMap::new();
    };

    let mut ancestor_id: Option<ClassId> = None;
    for base_ref in &class.base_refs {
        let was_imported = base_ref_head_is_imported(file, base_ref);
        match project::resolve_base_ref(project, file, base_ref) {
            Some(resolved) if ancestor_id.is_none() => {
                ancestor_id = Some(resolved);
            }
            Some(_) => {}
            None if was_imported => {
                diagnostics_by_file.entry(file.path.clone()).or_default().push(Diagnostic {
                    code: "DOC002",
                    severity: Severity::Info,
                    message: "base class import could not be statically resolved within the project \
                              (external/stdlib dependency, or genuinely not found) — treated as opaque"
                        .to_string(),
                    range: class.range,
                });
            }
            None => {}
        }
    }

    let parent_view: MethodViews = ancestor_id
        .map(|id| resolve_and_rewrite(project, &id, project_default_style, memo, edits_by_file, diagnostics_by_file))
        .unwrap_or_default();

    let edits = edits_by_file.entry(file.path.clone()).or_default();
    let diagnostics = diagnostics_by_file.entry(file.path.clone()).or_default();
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
                let style_name = effective_style_name(class, method_name, method, file.model.file_level_style.as_deref(), project_default_style);
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

                    for name in &effective_overrides {
                        if !method.signature.names.contains(name) {
                            diagnostics.push(Diagnostic {
                                code: "DOC007",
                                severity: Severity::Warning,
                                message: format!(
                                    "'override' names '{name}', which is not a parameter in this entity's signature"
                                ),
                                range: doc.inner_range,
                            });
                        }
                    }
                    if !effective_exclude.is_empty() && effective_expand_target.is_none() {
                        diagnostics.push(Diagnostic {
                            code: "DOC007",
                            severity: Severity::Warning,
                            message: "'exclude' has no effect without 'expand_kwargs'".to_string(),
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

    memo.insert(class_id.clone(), this_view.clone());
    this_view
}

/// Whether a base-class reference's leading name (the whole name for `BaseRef::Name`, the first
/// segment for `BaseRef::Attribute`) corresponds to an actual `import` statement in this file —
/// distinguishes "this was a deliberate reference to something outside the project" (worth a
/// quiet DOC002) from "this is just a builtin/not-tracked name" (worth nothing at all; `object`,
/// `Exception`, `dict`, etc. are never imported, and diagnosing every one of those would be pure
/// noise).
fn base_ref_head_is_imported(file: &ProjectFile, base_ref: &model::BaseRef) -> bool {
    let head = match base_ref {
        model::BaseRef::Name(name) => name.as_str(),
        model::BaseRef::Attribute(segments) => match segments.first() {
            Some(first) => first.as_str(),
            None => return false,
        },
    };
    file.model.imports.contains_key(head)
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

    fn sync_multi(files: &[(&str, &str)]) -> HashMap<String, FileOutput> {
        let owned: Vec<(PathBuf, String)> = files.iter().map(|(p, t)| (PathBuf::from(p), t.to_string())).collect();
        sync_project(&owned, None)
            .into_iter()
            .map(|out| (out.path.to_string_lossy().replace('\\', "/"), out))
            .collect()
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

    const BASE_PY: &str = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    arg1 : int
        Arg1 doc, from Base.
    \"\"\"

    def __init__(self, arg1):
        pass
";

    fn child_using(base_import: &str) -> String {
        format!(
            "\
{base_import}


class Child(Base):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should sync from Base.
    \"\"\"

    def __init__(self, arg1):
        pass
"
        )
    }

    #[test]
    fn resolves_base_class_through_relative_sibling_import() {
        let child = child_using("from .base import Base");
        let outputs = sync_multi(&[("pkg/__init__.py", ""), ("pkg/base.py", BASE_PY), ("pkg/child.py", &child)]);

        let child_out = &outputs["pkg/child.py"];
        assert!(child_out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", child_out.diagnostics);
        assert!(child_out.text.contains("Arg1 doc, from Base."));
        assert!(!child_out.text.contains("Stale text"));
    }

    #[test]
    fn resolves_base_class_through_relative_parent_package_import() {
        // pkg/sub/child.py reaching up two levels to pkg/base.py
        let child = child_using("from ..base import Base");
        let outputs = sync_multi(&[
            ("pkg/__init__.py", ""),
            ("pkg/base.py", BASE_PY),
            ("pkg/sub/__init__.py", ""),
            ("pkg/sub/child.py", &child),
        ]);

        let child_out = &outputs["pkg/sub/child.py"];
        assert!(child_out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", child_out.diagnostics);
        assert!(child_out.text.contains("Arg1 doc, from Base."));
    }

    #[test]
    fn resolves_base_class_through_absolute_import_in_src_layout() {
        // The package's own name ("mypkg") must be discovered despite the "src/" container
        // directory, since that's what the absolute import actually names.
        let child = child_using("from mypkg.base import Base");
        let outputs = sync_multi(&[
            ("src/mypkg/__init__.py", ""),
            ("src/mypkg/base.py", BASE_PY),
            ("src/mypkg/child.py", &child),
        ]);

        let child_out = &outputs["src/mypkg/child.py"];
        assert!(child_out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", child_out.diagnostics);
        assert!(child_out.text.contains("Arg1 doc, from Base."));
    }

    #[test]
    fn resolves_base_class_through_attribute_access_on_an_imported_module() {
        let child = child_using("import pkg.base");
        // `import pkg.base` then `class Child(pkg.base.Base):` -- rewrite the base line since
        // `child_using` wrote a bare `Base`.
        let child = child.replacen("class Child(Base):", "class Child(pkg.base.Base):", 1);
        let outputs = sync_multi(&[("pkg/__init__.py", ""), ("pkg/base.py", BASE_PY), ("pkg/child.py", &child)]);

        let child_out = &outputs["pkg/child.py"];
        assert!(child_out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", child_out.diagnostics);
        assert!(child_out.text.contains("Arg1 doc, from Base."));
    }

    #[test]
    fn follows_a_reexport_through_init_py() {
        // pkg/__init__.py re-exports Base from pkg/_internal.py; usage.py imports it from the
        // package itself, never knowing (or needing to know) where Base is really defined.
        let internal_py = BASE_PY; // defines `Base` directly
        let init_py = "from ._internal import Base\n";
        let usage_py = child_using("from pkg import Base");

        let outputs = sync_multi(&[
            ("pkg/__init__.py", init_py),
            ("pkg/_internal.py", internal_py),
            ("usage.py", &usage_py),
        ]);

        let usage_out = &outputs["usage.py"];
        assert!(usage_out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", usage_out.diagnostics);
        assert!(usage_out.text.contains("Arg1 doc, from Base."));
    }

    #[test]
    fn unresolvable_import_is_diagnosed_as_opaque_and_leaves_file_untouched() {
        let child = child_using("from numpy import ndarray");
        let child = child.replacen("class Child(Base):", "class Child(ndarray):", 1);
        let outputs = sync_multi(&[("usage.py", &child)]);

        let out = &outputs["usage.py"];
        assert_eq!(out.text, child);
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].code, "DOC002");
    }

    #[test]
    fn bare_unimported_base_name_like_a_builtin_is_silent() {
        // `class Child(Exception):` -- Exception was never imported, so this must NOT be
        // diagnosed (matches every real class inheriting from a builtin/stdlib type with no
        // import statement at all).
        let source = "\
class Child(Exception):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Documented.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let outputs = sync_multi(&[("usage.py", source)]);
        let out = &outputs["usage.py"];
        assert_eq!(out.text, source);
        assert!(out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", out.diagnostics);
    }

    #[test]
    fn override_naming_a_non_signature_parameter_is_diagnosed() {
        let source = "\
# docerator: override=not_a_real_param
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
        let (_output, diagnostics) = sync(source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC007");
        assert!(diagnostics[0].message.contains("not_a_real_param"));
    }

    #[test]
    fn exclude_without_expand_kwargs_is_diagnosed() {
        let source = "\
# docerator: exclude=arg2
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
        let (_output, diagnostics) = sync(source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC007");
    }

    fn owned_files(pairs: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
        pairs.iter().map(|(p, t)| (PathBuf::from(p), t.to_string())).collect()
    }

    fn text_for<'a>(outputs: &'a [FileOutput], path: &str) -> &'a str {
        &outputs.iter().find(|o| o.path == std::path::Path::new(path)).unwrap().text
    }

    #[test]
    fn cached_run_matches_uncached_run() {
        let files = owned_files(&[
            ("pkg/__init__.py", ""),
            ("pkg/base.py", BASE_PY),
            ("pkg/child.py", &child_using("from .base import Base")),
        ]);

        let plain = sync_project(&files, None);
        let mut cache = Cache::default();
        let cached = sync_project_with_cache(&files, None, &mut cache);

        assert_eq!(plain.len(), cached.len());
        for (a, b) in plain.iter().zip(cached.iter()) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.text, b.text);
            assert_eq!(a.diagnostics.len(), b.diagnostics.len());
        }
    }

    #[test]
    fn second_run_with_unchanged_files_reuses_cache_and_matches_first_run() {
        let files = owned_files(&[
            ("pkg/__init__.py", ""),
            ("pkg/base.py", BASE_PY),
            ("pkg/child.py", &child_using("from .base import Base")),
        ]);

        let mut cache = Cache::default();
        let first = sync_project_with_cache(&files, None, &mut cache);
        assert!(!cache.files.is_empty());

        let second = sync_project_with_cache(&files, None, &mut cache);
        assert_eq!(text_for(&first, "pkg/child.py"), text_for(&second, "pkg/child.py"));
        assert!(text_for(&second, "pkg/child.py").contains("Arg1 doc, from Base."));
    }

    #[test]
    fn cache_invalidates_a_descendant_when_only_the_ancestor_file_changes() {
        let base_v1 = BASE_PY;
        let base_v2 = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    arg1 : int
        UPDATED description, from Base v2.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let child = child_using("from .base import Base");

        // Round 1: nothing is cached yet, so child.py resyncs from its stale local text to
        // match base_v1 immediately, same as any uncached run.
        let files_v1 = owned_files(&[("pkg/__init__.py", ""), ("pkg/base.py", base_v1), ("pkg/child.py", &child)]);
        let mut cache = Cache::default();
        let first = sync_project_with_cache(&files_v1, None, &mut cache);
        assert!(text_for(&first, "pkg/child.py").contains("Arg1 doc, from Base."));

        // Round 2: only base.py's content changes. child.py's own text is byte-identical to
        // round 1, but its dependency fingerprint (base.py's hash) no longer matches, so it
        // must NOT be served from the stale cache entry -- it has to resync to base_v2.
        let files_v2 = owned_files(&[("pkg/__init__.py", ""), ("pkg/base.py", base_v2), ("pkg/child.py", &child)]);
        let second = sync_project_with_cache(&files_v2, None, &mut cache);

        let child_output = text_for(&second, "pkg/child.py");
        assert!(
            child_output.contains("UPDATED description, from Base v2."),
            "child should have resynced to the new ancestor text, got: {child_output}"
        );
        assert!(!child_output.contains("Stale text that should sync."));
    }

    #[test]
    fn cache_is_invalidated_when_project_default_style_changes() {
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
        let files = owned_files(&[("solo.py", source)]);

        let mut cache = Cache::default();
        let first = sync_project_with_cache(&files, Some("numpydoc"), &mut cache);
        assert!(first[0].diagnostics.is_empty());

        // A different project-wide style default must not silently reuse the old cache entry
        // (even though nothing here observably differs today, since only one style exists —
        // the global key changing is what's under test, not a behavior difference).
        let second = sync_project_with_cache(&files, Some("google"), &mut cache);
        assert_eq!(second[0].diagnostics.len(), 1);
        assert_eq!(second[0].diagnostics[0].code, "DOC011");
    }

    #[test]
    fn stale_cache_format_version_is_ignored() {
        let files = owned_files(&[("solo.py", BASE_PY)]);
        let mut cache = Cache {
            format_version: 999,
            tool_version: "0.0.0-bogus".to_string(),
            global_key: "irrelevant".to_string(),
            files: HashMap::new(),
        };
        // Must not panic or misbehave -- just falls back to a full, correct recompute.
        let outputs = sync_project_with_cache(&files, None, &mut cache);
        assert_eq!(outputs.len(), 1);
        assert_eq!(cache.format_version, cache::CACHE_FORMAT_VERSION);
    }
}
