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
use crate::provenance::{self, ProvenanceEntry, ProvenanceMode, ReconcileOutcome};
use crate::style::numpydoc::NumpydocStyle;
use crate::style::{Diagnostic, DocStyle, EntryOrigin, ParamEntry, Severity};

pub struct FileOutput {
    pub path: PathBuf,
    pub text: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// Engine-wide behavior knobs beyond the per-file/per-entity `# docerator:` directives — plain
/// fields rather than a builder since there are only a couple so far; grows here rather than as
/// more positional parameters on `sync_project`/`sync_project_with_cache` as new project-wide
/// options are added.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions<'a> {
    pub project_default_style: Option<&'a str>,
    /// When a class inherits a parameter that isn't documented locally and the docstring has no
    /// `Parameters` section at all to insert it into (`DOC010`), synthesize one from scratch
    /// instead of only diagnosing the gap. Off by default — creating a new section is a more
    /// opinionated, structural edit than this tool's usual "only ever resync what's already
    /// there" default, so it's opt-in.
    pub insert_missing_sections: bool,
    /// Whether (and how) to make an auto-managed parameter's ancestor visible in the source: a
    /// managed comment block after the docstring, a note inline in the copied text, or neither.
    /// Defaults to `Comment` (via `ProvenanceMode`'s own `Default`) — unlike
    /// `insert_missing_sections`, this is purely presentational (never changes what a docstring's
    /// own content says, just adds a note about where it came from), so it's on by default.
    pub provenance_mode: ProvenanceMode,
    /// When several consecutive, auto-managed (inherited, non-overridden) parameters share
    /// identical documentation (type, description, *and* origin), render them back out as one
    /// combined `nameA, nameB : shared type` line — mirroring `numpydoc`'s own convention for
    /// this, and how they were almost certainly documented in the ancestor to begin with —
    /// instead of duplicating the identical text once per name. Off by default: like
    /// `insert_missing_sections`, this restructures the shape of a `Parameters` section rather
    /// than only ever resyncing content in place, so it's opt-in.
    pub merge_shared_parameters: bool,
}

impl<'a> SyncOptions<'a> {
    pub fn with_style(project_default_style: Option<&'a str>) -> Self {
        Self {
            project_default_style,
            insert_missing_sections: false,
            provenance_mode: ProvenanceMode::default(),
            merge_shared_parameters: false,
        }
    }
}

/// Sync every class across every file in `files` against the whole project's inheritance graph.
/// `files` should already be `.py` files only — filtering out anything else (and walking a real
/// directory tree in the first place) is the CLI's job, not this library's.
pub fn sync_project(files: &[(PathBuf, String)], project_default_style: Option<&str>) -> Vec<FileOutput> {
    sync_project_with_options(files, SyncOptions::with_style(project_default_style))
}

/// Same as `sync_project`, but with the full `SyncOptions` set rather than just a style default.
pub fn sync_project_with_options(files: &[(PathBuf, String)], options: SyncOptions) -> Vec<FileOutput> {
    sync_project_inner(files, options, None).0
}

/// Same as `sync_project`, but consults `cache` for files whose content — and whose entire
/// transitive ancestor chain's content — are unchanged since the run that produced it, skipping
/// the numpydoc-parsing/MRO-merge/diagnostic-generation work for those files entirely (see
/// `cache.rs` for exactly what is and isn't skipped, and why). `cache` is replaced with the
/// updated cache reflecting this run — the caller is responsible for persisting it.
pub fn sync_project_with_cache(files: &[(PathBuf, String)], project_default_style: Option<&str>, cache: &mut Cache) -> Vec<FileOutput> {
    sync_project_with_cache_and_options(files, SyncOptions::with_style(project_default_style), cache)
}

/// Same as `sync_project_with_cache`, but with the full `SyncOptions` set rather than just a
/// style default.
pub fn sync_project_with_cache_and_options(files: &[(PathBuf, String)], options: SyncOptions, cache: &mut Cache) -> Vec<FileOutput> {
    let (outputs, new_cache) = sync_project_inner(files, options, Some(&*cache));
    *cache = new_cache;
    outputs
}

fn sync_project_inner(files: &[(PathBuf, String)], options: SyncOptions, old_cache: Option<&Cache>) -> (Vec<FileOutput>, Cache) {
    let mut diagnostics_by_file: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
    let project = project::build_project(files, &mut diagnostics_by_file);
    let file_by_path: HashMap<PathBuf, &ProjectFile> = project.files.iter().map(|f| (f.path.clone(), f)).collect();
    // Comment-mode provenance reconciliation needs to read the file's raw text *after* a
    // docstring's own closing quotes -- `doc.text`/`inner_range` only ever cover the docstring's
    // own interior, and `ProjectFile` doesn't retain the whole file's source once parsed. This is
    // the one place `resolve_and_rewrite` needs it, so it's looked up by path rather than
    // threading a full copy through the model.
    let source_by_path: HashMap<&PathBuf, &str> = files.iter().map(|(p, t)| (p, t.as_str())).collect();

    let content_hash: HashMap<PathBuf, String> = files.iter().map(|(p, t)| (p.clone(), cache::hash_text(t))).collect();
    let module_hash: HashMap<String, String> = project
        .files
        .iter()
        .filter_map(|f| content_hash.get(&f.path).map(|h| (f.module_name.clone(), h.clone())))
        .collect();
    let module_names: Vec<String> = project.files.iter().map(|f| f.module_name.clone()).collect();
    let global_key = cache::compute_global_key(
        options.project_default_style,
        options.insert_missing_sections,
        options.provenance_mode,
        options.merge_shared_parameters,
        &module_names,
    );

    let usable_old_cache = old_cache.filter(|c| !c.is_stale() && c.global_key == global_key);

    let mut memo: HashMap<ClassId, MethodViews> = HashMap::new();
    let mut local_authored_memo: HashMap<ClassId, MethodViews> = HashMap::new();
    let mut mro_memo: HashMap<ClassId, Vec<ClassId>> = HashMap::new();
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
                for (class_name, cached_views) in &cached_file.local_authored {
                    let id = ClassId {
                        module: file.module_name.clone(),
                        name: class_name.clone(),
                    };
                    local_authored_memo.insert(id, cache::cached_to_method_views(cached_views));
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
        resolve_and_rewrite(
            &project,
            id,
            options.project_default_style,
            options.insert_missing_sections,
            options.provenance_mode,
            options.merge_shared_parameters,
            &source_by_path,
            &mut mro_memo,
            &mut local_authored_memo,
            &mut memo,
            &mut edits_by_file,
            &mut diagnostics_by_file,
        );
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
            let local_authored: HashMap<String, cache::CachedMethodViews> = file
                .model
                .classes
                .keys()
                .map(|name| {
                    let id = ClassId {
                        module: file.module_name.clone(),
                        name: name.clone(),
                    };
                    let views = local_authored_memo.get(&id).cloned().unwrap_or_default();
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
                    local_authored,
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
    sync_source_with_options(source, SyncOptions::with_style(project_default_style))
}

/// Same as `sync_source`, but with the full `SyncOptions` set rather than just a style default.
pub fn sync_source_with_options(
    source: &str,
    options: SyncOptions,
) -> Result<(String, Vec<Diagnostic>), ruff_python_parser::ParseError> {
    parse::parse(source)?;
    let path = PathBuf::from("<source>.py");
    let files = [(path.clone(), source.to_string())];
    let mut outputs = sync_project_with_options(&files, options);
    let output = outputs.pop().expect("sync_project_with_options returns one output per input file");
    Ok((output.text, output.diagnostics))
}

/// Per method name, the full flattened "most-derived-that-defines-it wins" view of every entry
/// authored anywhere in the ancestor chain up to and including this class — what a descendant
/// sees when it looks up this class as an ancestor.
type MethodViews = IndexMap<String, IndexMap<String, ParamEntry>>;

#[allow(clippy::too_many_arguments)]
fn resolve_and_rewrite(
    project: &ProjectModel,
    class_id: &ClassId,
    project_default_style: Option<&str>,
    insert_missing_sections: bool,
    provenance_mode: ProvenanceMode,
    merge_shared_parameters: bool,
    source_by_path: &HashMap<&PathBuf, &str>,
    mro_memo: &mut HashMap<ClassId, Vec<ClassId>>,
    local_authored_memo: &mut HashMap<ClassId, MethodViews>,
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

    // Diagnose an unresolvable-but-imported *direct* base ref -- unchanged, still about the
    // literal syntax on this class's own `class Foo(...):` line, independent of the transitive
    // MRO computed below.
    for base_ref in &class.base_refs {
        let was_imported = base_ref_head_is_imported(file, base_ref);
        if project::resolve_base_ref(project, file, base_ref).is_none() && was_imported {
            diagnostics_by_file.entry(file.path.clone()).or_default().push(Diagnostic {
                code: "DOC002",
                severity: Severity::Info,
                message: "base class import could not be statically resolved within the project \
                          (external/stdlib dependency, or genuinely not found) — treated as opaque"
                    .to_string(),
                range: class.range,
            });
        }
    }

    // Real C3 linearization (matching Python's own MRO algorithm), not an approximation of it:
    // walk this class's full transitive ancestor order (excluding itself) in *reverse* priority
    // (least-specific ancestor first, most-specific last), overlaying each ancestor's own LOCAL
    // contribution only -- never an already-flattened view. That distinction is what makes this
    // correct for genuine multiple inheritance: an ancestor that merely *passes through* some
    // parameter (never documents it itself) must never be able to shadow a more-specific class
    // elsewhere in the MRO that actually redocuments it, which a naive "merge each direct base's
    // own fully-resolved view" scheme (this function's own earlier implementation) gets wrong
    // whenever a shared ancestor is reached through one branch that doesn't override some
    // parameter and another branch that does -- see `compute_mro`'s own doc comment for the
    // worked example and why it matters.
    let mro = compute_mro(project, class_id, mro_memo, diagnostics_by_file);
    let mut parent_view: MethodViews = IndexMap::new();
    for ancestor_id in mro[1..].iter().rev() {
        resolve_and_rewrite(
            project,
            ancestor_id,
            project_default_style,
            insert_missing_sections,
            provenance_mode,
            merge_shared_parameters,
            source_by_path,
            mro_memo,
            local_authored_memo,
            memo,
            edits_by_file,
            diagnostics_by_file,
        );
        if let Some(ancestor_local) = local_authored_memo.get(ancestor_id) {
            for (method_name, params) in ancestor_local {
                let target = parent_view.entry(method_name.clone()).or_default();
                for (param_name, entry) in params {
                    target.insert(param_name.clone(), entry.clone());
                }
            }
        }
    }

    let edits = edits_by_file.entry(file.path.clone()).or_default();
    let diagnostics = diagnostics_by_file.entry(file.path.clone()).or_default();
    // Seed from the ancestor's own resolved view *before* overlaying this class's locally
    // defined methods, so a method this class doesn't override at all (e.g. `class Parent(GrandParent): pass`,
    // no `__init__` of its own) stays transparently visible to further descendants — matching
    // Python's real MRO, where such a class doesn't hide `GrandParent.__init__` from `Child`.
    // Without this, a non-overriding ancestor would silently erase every parameter documented
    // above it in the chain.
    let mut this_view: MethodViews = parent_view.clone();
    // This class's own contribution only (never pass-through) -- what a descendant elsewhere in
    // the MRO needs to correctly overlay on top of *its* inherited view, without also dragging
    // along whatever this class merely forwards from its own ancestors (which would let a
    // pass-through wrongly shadow a more-specific override reached via a different MRO branch).
    let mut local_authored_for_this_class: MethodViews = IndexMap::new();

    // A class that documents constructor parameters in its own class-level docstring, purely by
    // numpydoc convention, without itself declaring `__init__` at all -- inheriting the real
    // constructor unchanged from an ancestor -- is otherwise completely invisible to this whole
    // engine: `class.methods` has no `"__init__"` entry to iterate below at all, so nothing ever
    // parses, checks, or resyncs its docstring, silently, forever. Real-world SimPEG examples:
    // `TimeFields(Fields)` and `Simulation3DElectricField(BaseFDEMSimulation)`, both `pass`-style
    // (no `__init__` of their own) with a full `Parameters` section describing the inherited
    // constructor anyway. Synthesize a virtual `__init__` for exactly this case: borrow the
    // signature of the nearest ancestor (walking the real MRO, not just the direct base) that
    // actually defines `__init__` locally -- that's the constructor Python really calls for this
    // class, unchanged -- and process this class's own docstring against it, same as any other
    // method. `directives: Directives::default()` is correct, not a placeholder: there's no `def
    // __init__` line in the source to attach a directive comment above in the first place, so
    // there's nothing to parse there -- `effective_overrides`/`effective_style_name`/etc. already
    // fall back to the class's own directives for `__init__` specifically, which is exactly what
    // should govern this synthesized entry too.
    let methods: std::borrow::Cow<IndexMap<String, MethodModel>> = if class.methods.contains_key("__init__") {
        std::borrow::Cow::Borrowed(&class.methods)
    } else if let Some(docstring) = &class.own_docstring {
        let borrowed_signature = mro[1..].iter().find_map(|ancestor_id| {
            project
                .class(ancestor_id)
                .and_then(|(_, ancestor_class)| ancestor_class.methods.get("__init__"))
                .map(|m| m.signature.clone())
        });
        match borrowed_signature {
            Some(signature) => {
                let mut augmented = class.methods.clone();
                augmented.insert(
                    "__init__".to_string(),
                    MethodModel {
                        docstring: Some(docstring.clone()),
                        signature,
                        directives: crate::directives::Directives::default(),
                    },
                );
                std::borrow::Cow::Owned(augmented)
            }
            None => std::borrow::Cow::Borrowed(&class.methods),
        }
    } else {
        std::borrow::Cow::Borrowed(&class.methods)
    };

    for (method_name, method) in methods.iter() {
        let inherited_from_ancestors = parent_view.get(method_name).cloned().unwrap_or_default();
        let effective_skip = class.directives.skip || method.directives.skip;
        let effective_overrides = effective_overrides(class, method_name, method);
        let effective_exclude = effective_exclude(class, method_name, method);
        let effective_expand_target = effective_expand_kwargs_into(class, method_name, method);
        let mut authored: IndexMap<String, ParamEntry> = IndexMap::new();

        if let Some(doc) = &method.docstring {
            {
                let style_name = effective_style_name(class, method_name, method, file.model.file_level_style.as_deref(), project_default_style);
                let style = resolve_style(&style_name, doc.inner_range, diagnostics);
                let (mut parsed_entries, parse_diagnostics) = style.parse_entries(&doc.text);
                // Docerator never decodes escape sequences -- it always splices raw source
                // bytes, so a `\` by itself is harmless. The only real hazard is copying text
                // *between* two docstrings with different raw-ness (`r"""..."""` vs `"""..."""`),
                // where the same bytes carry different escape semantics -- guarded per-entry,
                // right where that copying actually happens (`format_entry` call sites below),
                // not by refusing to touch an entire docstring for containing a `\` anywhere.
                parsed_entries.set_is_raw(doc.is_raw);
                // Every entry parsed here is, as of this moment, "authored in this class" as
                // far as provenance is concerned -- if it turns out to be pure pass-through
                // inheritance instead (not added to `authored` below), it never overwrites
                // `this_view`'s existing copy, so a deeper ancestor's own stamp survives
                // untouched. That's what lets a stamp made once here answer "who really first
                // authored this" correctly at any depth, with no extra bookkeeping.
                parsed_entries.set_origin(EntryOrigin {
                    module: class_id.module.clone(),
                    class_name: class_id.name.clone(),
                });

                if effective_skip {
                    // Exempt: no edits, no diagnostics about this entity's own structure — but
                    // its content still flows to descendants exactly as authored elsewhere.
                    for (name, entry) in parsed_entries.iter() {
                        authored.insert(name.clone(), entry.clone());
                    }
                } else {
                    // Accumulated across every render site below (the primary/secondary rebuild,
                    // `insert_missing_sections`'s synthesis, and `expand_kwargs`) and reconciled
                    // into a comment block once, after all of them, at the very end of this
                    // method's processing -- recorded whenever a copied entry is actually used
                    // for splicing, *including* the already-in-sync case, so the block stays
                    // correct on every run, not only ones that also produce a text edit.
                    let mut provenance_entries: Vec<ProvenanceEntry> = Vec::new();

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

                    // Rebuild the whole `Parameters` block in signature order every run, rather
                    // than splicing each entry in place / appending new ones at the end — a
                    // per-entry edit can only say "replace this text" or "insert at one fixed
                    // point," neither of which can express "this entry needs to move before
                    // one that's already there," which is exactly what's needed both for a
                    // freshly-auto-filled entry (it might belong in the middle of the section,
                    // not at the end) and for pre-existing entries a human wrote in some other
                    // order (extremely common in code that predates this tool). Comparing the
                    // freshly-built block against what's already there before emitting an edit
                    // keeps this a no-op whenever nothing actually needs to change.
                    let mut ordered_names: Vec<String> = Vec::new();
                    for name in &method.signature.names {
                        if effective_overrides.contains(name) {
                            if parsed_entries.primary.contains_key(name) {
                                ordered_names.push(name.clone());
                            }
                            continue;
                        }
                        if inherited_from_ancestors.contains_key(name) || parsed_entries.primary.contains_key(name) {
                            ordered_names.push(name.clone());
                        } else {
                            diagnostics.push(Diagnostic {
                                code: "DOC001",
                                severity: Severity::Warning,
                                message: format!(
                                    "parameter '{name}' is not documented locally and no ancestor documents it"
                                ),
                                range: doc.inner_range,
                            });
                        }
                    }
                    // Anything documented locally but not actually part of the signature (a
                    // stray/legacy entry) keeps its original relative order, appended after
                    // every signature-ordered entry -- there's no signature position for it to
                    // match, so "leave it where it already reads naturally" is the only sane
                    // default. EXCEPT a name that's currently `expand_kwargs`-eligible (inherited,
                    // not overridden, not excluded) while the *active* target is `Other
                    // Parameters` -- that name belongs in the other section now, not here, most
                    // commonly because the `expand_kwargs=` directive's value just changed since
                    // the last run. Leaving it out of `ordered_names` (and therefore out of the
                    // rebuilt `pieces` below) is what actually removes it from this block: the
                    // whole-block rebuild still replaces `block_range` (computed from every entry
                    // literally on disk, this one included) with freshly built content that no
                    // longer mentions it -- no separate delete edit needed for this direction.
                    // `expand_kwargs` itself is responsible for the *other* direction (a stale
                    // entry left behind in `Other Parameters` when the target switches away from
                    // it), since nothing else ever manages that section.
                    for name in parsed_entries.primary.keys() {
                        if effective_expand_target == Some(crate::style::ParamSectionKind::Secondary)
                            && inherited_from_ancestors.contains_key(name)
                            && !effective_overrides.contains(name)
                            && !effective_exclude.contains(name)
                        {
                            continue;
                        }
                        if !ordered_names.iter().any(|n| n == name) {
                            ordered_names.push(name.clone());
                        }
                    }

                    match primary_block_span(&parsed_entries.primary, &doc.text) {
                        Some((block_range, indent)) => {
                            let newline = newline_style(&doc.text);
                            // `merge_shared_parameters` collapses a run of consecutive,
                            // auto-managed names sharing identical documentation into one group,
                            // rendered as a single `nameA, nameB : shared type` line below --
                            // every other name is its own singleton group, so this is a no-op
                            // shape-wise when the option is off.
                            let groups =
                                group_names_for_rendering(&ordered_names, &effective_overrides, &inherited_from_ancestors, merge_shared_parameters);
                            let mut pieces: Vec<String> = Vec::with_capacity(groups.len());
                            // Parallel to `pieces`, not `ordered_names`/`groups` -- a comma-group
                            // entry (`nameA, nameB : shared type`, on-disk or newly merged) can
                            // make those list more names than `pieces` ends up with actual rows
                            // for, so the join loop needs its own aligned name list to look up
                            // gaps against.
                            let mut piece_names: Vec<&String> = Vec::with_capacity(groups.len());
                            // The most recent *authored* (verbatim on-disk) entry's own range, so
                            // a `numpydoc` `nameA, nameB : shared type` comma-group -- which
                            // `parse_section` gives every one of its names the identical `range`
                            // -- only ever contributes its shared text once, at its first name,
                            // instead of once per name (which would duplicate that whole line for
                            // every comma-separated name sharing it). Reset on anything that
                            // isn't itself an authored continuation of the same range, so an
                            // ancestor-copied name in between never gets bridged across.
                            let mut last_authored_range: Option<TextRange> = None;
                            for group in &groups {
                                let representative = group[0];
                                let mut used_ancestor = false;
                                if !effective_overrides.contains(representative) {
                                    if let Some(ancestor_entry) = inherited_from_ancestors.get(representative) {
                                        let joined_names: String =
                                            group.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ");
                                        let candidate = crate::style::numpydoc::render_entry_text(&joined_names, ancestor_entry, newline);
                                        if safe_to_copy_across_raw_ness(ancestor_entry.is_raw, doc.is_raw, &candidate) {
                                            if let Some(origin) = &ancestor_entry.origin {
                                                for name in group {
                                                    provenance_entries.push(ProvenanceEntry {
                                                        name: (*name).clone(),
                                                        origin: origin.clone(),
                                                    });
                                                }
                                            }
                                            pieces.push(provenance::maybe_append_inline_note(
                                                candidate,
                                                &indent,
                                                newline,
                                                provenance_mode,
                                                ancestor_entry.origin.as_ref(),
                                            ));
                                            piece_names.push(representative);
                                            last_authored_range = None;
                                            used_ancestor = true;
                                        } else {
                                            for name in group {
                                                diagnostics.push(backslash_raw_mismatch_diagnostic(name, doc.inner_range));
                                            }
                                        }
                                    }
                                }
                                if !used_ancestor {
                                    // Authored (overridden, locally-new, or a non-signature
                                    // extra): preserve the exact on-disk text untouched. A group
                                    // here is always a singleton -- `group_names_for_rendering`
                                    // only ever merges names that passed the ancestor-safe check
                                    // above -- but every member still needs its own turn in case
                                    // the check above failed for what would otherwise have been a
                                    // multi-name group.
                                    for name in group {
                                        match parsed_entries.primary.get(*name) {
                                            Some(existing) => {
                                                if last_authored_range == Some(existing.range) {
                                                    continue; // comma-group continuation -- nothing more to emit
                                                }
                                                last_authored_range = Some(existing.range);
                                                pieces.push(
                                                    doc.text[usize::from(existing.range.start())..usize::from(existing.range.end())]
                                                        .to_string(),
                                                );
                                                piece_names.push(name);
                                            }
                                            None => {
                                                pieces.push(String::new());
                                                piece_names.push(name);
                                                last_authored_range = None;
                                            }
                                        }
                                    }
                                }
                            }

                            let mut new_block = String::new();
                            for (i, piece) in pieces.iter().enumerate() {
                                if i > 0 {
                                    // Two entries that were already directly adjacent, in the
                                    // same order, keep whatever gap originally separated them
                                    // (including any blank line an author put there) — the tool
                                    // never touches formatting it didn't need to move. Only a
                                    // pair that's actually being reordered around (at least one
                                    // side didn't exist before, or they weren't neighbors) falls
                                    // back to the tool's own plain single-newline separator.
                                    let gap = original_gap_if_still_adjacent(
                                        piece_names[i - 1],
                                        piece_names[i],
                                        &parsed_entries.primary,
                                        &doc.text,
                                    )
                                    .unwrap_or_else(|| format!("{newline}{indent}"));
                                    new_block.push_str(&gap);
                                }
                                new_block.push_str(piece);
                            }

                            let current_block =
                                &doc.text[usize::from(block_range.start())..usize::from(block_range.end())];
                            if new_block != current_block {
                                edits.push(TextEdit::new(offset_range(block_range, doc.inner_range.start()), new_block));
                            }
                        }
                        None => {
                            let missing_names: Vec<&String> = ordered_names
                                .iter()
                                .filter(|name| {
                                    !effective_overrides.contains(*name)
                                        && inherited_from_ancestors.contains_key(*name)
                                        && !parsed_entries.primary.contains_key(*name)
                                })
                                .collect();

                            // `insert_missing_sections` is only trusted when there's no
                            // `Parameters` header at all (`primary_block_span` also returns
                            // `None` for a *present* header whose entries came out malformed or
                            // non-monotonic -- see its own doc comment -- and synthesizing a
                            // second header on top of either would either duplicate it outright
                            // or splice against bookkeeping we've already decided not to trust),
                            // and only when `margin_indent` is non-empty: an empty margin means
                            // `compute_margin` had nothing indented to measure at all (most
                            // commonly a single-line docstring, `"""Just a summary."""`, with no
                            // other content whose indentation a synthesized section could borrow)
                            // -- inserting at column 0 would produce a section visibly misindented
                            // relative to the rest of the docstring, worse than just diagnosing.
                            if insert_missing_sections
                                && !missing_names.is_empty()
                                && !parsed_entries.has_primary_section
                                && !parsed_entries.margin_indent.is_empty()
                            {
                                let newline = newline_style(&doc.text);
                                let indent = &parsed_entries.margin_indent;
                                let raw_header = style.synthesize_section(crate::style::ParamSectionKind::Primary);
                                let indented_header: String =
                                    raw_header.lines().map(|line| format!("{indent}{line}{newline}")).collect();

                                let mut body = String::new();
                                for (i, name) in missing_names.iter().enumerate() {
                                    if i > 0 {
                                        body.push_str(newline);
                                    }
                                    body.push_str(indent);
                                    let ancestor_entry = inherited_from_ancestors.get(*name).expect("filtered to inherited names above");
                                    let rendered = style.format_entry(ancestor_entry, "", newline);
                                    if let Some(origin) = &ancestor_entry.origin {
                                        provenance_entries.push(ProvenanceEntry {
                                            name: (*name).clone(),
                                            origin: origin.clone(),
                                        });
                                    }
                                    body.push_str(&provenance::maybe_append_inline_note(
                                        rendered,
                                        indent,
                                        newline,
                                        provenance_mode,
                                        ancestor_entry.origin.as_ref(),
                                    ));
                                }

                                let (insertion_point, text) = match parsed_entries.first_section_start {
                                    // Inserting directly ahead of an existing section: its own
                                    // leading blank line (already in the source, right before
                                    // this offset) becomes the separator from our new entries,
                                    // so we only need a trailing blank line of our own.
                                    Some(offset) => (offset, format!("{indented_header}{body}{newline}{newline}")),
                                    // No other section exists at all: append at the end of the
                                    // docstring's own content, providing our own leading blank
                                    // line since nothing else supplies one.
                                    None => (doc.text.trim_end().len(), format!("{newline}{newline}{indented_header}{body}")),
                                };
                                let at = doc.inner_range.start() + TextSize::try_from(insertion_point).unwrap();
                                edits.push(TextEdit::new(TextRange::new(at, at), text));
                            } else {
                                for name in missing_names {
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
                            }
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
                                doc.is_raw,
                                doc.inner_range,
                                &style,
                                provenance_mode,
                                &mut provenance_entries,
                                edits,
                                diagnostics,
                            );
                        }
                    }

                    {
                        // Always reconciled, not just under `Comment` -- a mode switch away from
                        // `Comment` must still clean up a block a previous run left behind, which
                        // only happens if this runs every time with an empty entries list (the
                        // "delete" branch), rather than skipping the whole reconciliation outright.
                        let entries_if_comment_mode: &[ProvenanceEntry] =
                            if provenance_mode == ProvenanceMode::Comment { &provenance_entries } else { &[] };
                        let source = source_by_path.get(&file.path).copied().unwrap_or_default();
                        let newline = newline_style(&doc.text);
                        match provenance::reconcile(
                            source,
                            doc.literal_range.end(),
                            &parsed_entries.margin_indent,
                            newline,
                            entries_if_comment_mode,
                        ) {
                            ReconcileOutcome::NoOp => {}
                            ReconcileOutcome::Edit(edit) => edits.push(edit),
                            ReconcileOutcome::Blocked => {
                                diagnostics.push(Diagnostic {
                                    code: "DOC013",
                                    severity: Severity::Warning,
                                    message: "provenance comment could not be inserted: the \
                                              docstring shares its closing line with other code \
                                              (e.g. a `;`-chained statement), which a `#` \
                                              comment inserted there would silently comment out"
                                        .to_string(),
                                    range: doc.inner_range,
                                });
                            }
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

        local_authored_for_this_class.insert(method_name.clone(), authored.clone());

        let mut resolved = inherited_from_ancestors;
        for (name, entry) in authored {
            resolved.insert(name, entry);
        }
        this_view.insert(method_name.clone(), resolved);
    }

    local_authored_memo.insert(class_id.clone(), local_authored_for_this_class);
    memo.insert(class_id.clone(), this_view.clone());
    this_view
}

/// `class_id`'s own Method Resolution Order, via the same C3 linearization algorithm real Python
/// uses (`L[C] = C + merge(L[B1], L[B2], ..., L[Bn], [B1, B2, ..., Bn])`) — the project-wide,
/// fully transitive priority order later used to decide, for any parameter documented by more
/// than one ancestor, which one actually wins. Returns `[class_id, most-specific ancestor, ...,
/// least-specific ancestor]`; a caller that only wants the ancestor portion skips index 0.
/// Memoized per class (the same shared ancestor's MRO would otherwise be recomputed once per
/// descendant that reaches it).
///
/// This is worth doing properly rather than approximating, because the two approaches can give
/// genuinely different answers, not just different code paths to the same result. Worked example
/// (the textbook case C3 exists to handle): `Grandparent` documents `x`; `BranchA(Grandparent)`
/// redocuments `x` with its own text; `BranchB(Grandparent)` doesn't touch `x` at all (pure
/// pass-through); `Child(BranchB, BranchA)`. True MRO is `Child, BranchB, BranchA, Grandparent`
/// — `BranchB` doesn't define `x` itself, so resolution continues past it to `BranchA`, whose own
/// redocumented `x` wins. A scheme that instead merges each *direct* base's own already-flattened
/// view (this function's own predecessor) can't tell "BranchB's `x` is really Grandparent's,
/// merely passed through" from "BranchB's `x` is BranchB's own" — both look identical once
/// flattened — so it has no way to prefer BranchA's more-specific redefinition over what's
/// actually just Grandparent's value arriving via BranchB. Walking the true MRO and overlaying
/// only each ancestor's own *local* contribution (see `resolve_and_rewrite`'s use of
/// `local_authored_memo`, never a flattened view) is what avoids that.
fn compute_mro(
    project: &ProjectModel,
    class_id: &ClassId,
    memo: &mut HashMap<ClassId, Vec<ClassId>>,
    diagnostics_by_file: &mut HashMap<PathBuf, Vec<Diagnostic>>,
) -> Vec<ClassId> {
    if let Some(cached) = memo.get(class_id) {
        return cached.clone();
    }
    // Placeholder guarding against unbounded recursion on a cyclic base chain -- invalid Python,
    // but a static tool must not infinitely recurse on malformed/adversarial input either way.
    memo.insert(class_id.clone(), vec![class_id.clone()]);

    let Some((file, class)) = project.class(class_id) else {
        let result = vec![class_id.clone()];
        memo.insert(class_id.clone(), result.clone());
        return result;
    };

    let resolved_bases: Vec<ClassId> = class
        .base_refs
        .iter()
        .filter_map(|base_ref| project::resolve_base_ref(project, file, base_ref))
        .collect();
    let base_mros: Vec<Vec<ClassId>> = resolved_bases.iter().map(|b| compute_mro(project, b, memo, diagnostics_by_file)).collect();

    let ancestors = c3_merge(&base_mros, &resolved_bases).unwrap_or_else(|| {
        // Real Python would itself refuse to construct this class (`TypeError: Cannot create a
        // consistent method resolution order`) -- but docerator operates statically and can't
        // know whether a class shaped like this is ever actually instantiated, so it degrades
        // gracefully (declaration-order-ish best effort) with a diagnostic instead of erroring
        // out or panicking.
        diagnostics_by_file.entry(file.path.clone()).or_default().push(Diagnostic {
            code: "DOC014",
            severity: Severity::Warning,
            message: "base classes have an inconsistent order (Python itself would refuse to \
                      construct this hierarchy); falling back to declaration order for inherited \
                      parameter documentation"
                .to_string(),
            range: class.range,
        });
        let mut fallback = Vec::new();
        for base_mro in &base_mros {
            for c in base_mro {
                if !fallback.contains(c) {
                    fallback.push(c.clone());
                }
            }
        }
        fallback
    });

    let mut result = vec![class_id.clone()];
    result.extend(ancestors);
    memo.insert(class_id.clone(), result.clone());
    result
}

/// The core C3 `merge` step: repeatedly takes the first head among `base_mros`/`bases_in_order`
/// that doesn't appear in the *tail* of any of the others, appends it to the result, and removes
/// it everywhere, until every list is exhausted. `None` if no valid head can ever be found at
/// some step — an inconsistent hierarchy, per C3's own consistency requirement.
fn c3_merge(base_mros: &[Vec<ClassId>], bases_in_order: &[ClassId]) -> Option<Vec<ClassId>> {
    let mut lists: Vec<Vec<ClassId>> = base_mros.to_vec();
    lists.push(bases_in_order.to_vec());
    let mut result = Vec::new();
    loop {
        lists.retain(|l| !l.is_empty());
        if lists.is_empty() {
            return Some(result);
        }
        let selected = lists.iter().find_map(|candidate_list| {
            let candidate = &candidate_list[0];
            let in_any_tail = lists.iter().any(|l| l.len() > 1 && l[1..].contains(candidate));
            (!in_any_tail).then(|| candidate.clone())
        })?;
        result.push(selected.clone());
        for l in &mut lists {
            l.retain(|c| c != &selected);
        }
    }
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
/// content in this section — either the last real entry in `section_entries`, or a trailing
/// `*args`/`**kwargs` run past it (`trailing_var_args`), whichever genuinely ends later — plus
/// the leading indentation to prepend to a newly-inserted line (a brand-new line has no existing
/// indentation of its own to reuse, unlike a splice-replaced entry) — borrowed from whichever of
/// the two anchors won, on the assumption every entry/var-args line in a section shares one
/// indentation level (true for any well-formed numpydoc section). `None` when the section has
/// neither real entries nor a trailing var-args run — there's nothing to anchor to yet.
///
/// A trailing `*args`/`**kwargs` line is never itself a `ParamEntry` (see
/// `ParsedEntries::primary_trailing_var_args`), so anchoring off `section_entries` alone always
/// lands *before* it when it's genuinely last — corrupting a freshly-synthesized section header,
/// or a newly-appended entry, into the middle of the author's own `**kwargs` documentation.
fn append_point(
    section_entries: &IndexMap<String, ParamEntry>,
    trailing_var_args: Option<TextRange>,
    docstring_text: &str,
) -> Option<(usize, String)> {
    let last_named = section_entries.values().max_by_key(|e| e.range.end());
    let (anchor_start, anchor_end) = match (last_named, trailing_var_args) {
        (Some(named), Some(var_args)) if named.range.end() >= var_args.end() => {
            (named.range.start(), named.range.end())
        }
        (Some(_), Some(var_args)) => (var_args.start(), var_args.end()),
        (Some(named), None) => (named.range.start(), named.range.end()),
        (None, Some(var_args)) => (var_args.start(), var_args.end()),
        (None, None) => return None,
    };
    let indent = line_indent_before(docstring_text, usize::from(anchor_start));
    Some((usize::from(anchor_end), indent))
}

/// The whitespace between the start of `offset`'s own line and `offset` itself — used to
/// recover a line's indentation from an entry's own `range.start()` (which, by design, always
/// starts right after that indentation, not before it).
fn line_indent_before(docstring_text: &str, offset: usize) -> String {
    let line_start = docstring_text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    docstring_text[line_start..offset].to_string()
}

/// Walks backward from `line_start` (which must be the start of some physical line) past any
/// number of blank lines, returning the byte offset right after the nearest non-blank line's own
/// content (before its terminator, `\r` included if present) — or `0` if everything before
/// `line_start` is blank. This is the position a "delete a whole managed block, including the
/// blank-line gap that separated it from whatever precedes it" edit should start from: deleting
/// from here through the block's own end reconnects the surrounding text seamlessly (the
/// preceding content's own line, followed directly by whatever originally followed the block),
/// instead of leaving a dangling blank line where the block used to be. Mirrors `provenance.rs`'s
/// `reconcile` delete case, which solves the same problem for a single known anchor point instead
/// of a general backward scan.
fn end_of_content_before(docstring_text: &str, mut line_start: usize) -> usize {
    loop {
        if line_start == 0 {
            return 0;
        }
        let search_end = line_start - 1; // the '\n' terminating the line right before `line_start`
        let prev_line_start = docstring_text[..search_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let prev_line_content = docstring_text[prev_line_start..search_end].trim_end_matches('\r');
        if prev_line_content.trim().is_empty() {
            line_start = prev_line_start;
            continue;
        }
        return prev_line_start + prev_line_content.len();
    }
}

/// `"\r\n"` if `docstring_text` already uses CRLF line endings anywhere, else plain `"\n"` — for
/// a separator the tool synthesizes itself (no original text to copy the terminator from). Only
/// ever needed when reusing an *existing* gap verbatim isn't possible (see
/// `original_gap_if_still_adjacent`); every other line ending in a rewritten docstring comes
/// from either untouched surrounding text or a raw entry slice, both already carrying whatever
/// terminator the source file actually uses. Getting this wrong doesn't corrupt content, but it
/// does quietly mix line-ending conventions within one file wherever it fires.
fn newline_style(docstring_text: &str) -> &'static str {
    if docstring_text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Whether `rendered` (an ancestor entry's own formatted text) is safe to splice, as-is, into a
/// docstring whose own raw-ness is `target_is_raw`. Docerator always splices raw source bytes and
/// never decodes escapes, so text moving between two docstrings of the *same* raw-ness carries
/// identical escape semantics on both sides regardless of what characters it contains — a `\` is
/// only ever a hazard when raw-ness actually *differs* (the same bytes would mean something
/// different re-interpreted in the other kind of literal) and the text actually contains one.
fn safe_to_copy_across_raw_ness(ancestor_is_raw: bool, target_is_raw: bool, rendered: &str) -> bool {
    ancestor_is_raw == target_is_raw || !rendered.contains('\\')
}

/// The DOC008 diagnostic for a specific parameter whose ancestor-regenerated text was withheld
/// by `safe_to_copy_across_raw_ness` — narrower than the blanket "docstring contains a backslash"
/// refusal this replaced: only the one unsafe *entry* is left untouched (whatever's already on
/// disk, or simply not inserted), not the whole docstring's worth of otherwise-unrelated entries.
fn backslash_raw_mismatch_diagnostic(name: &str, range: TextRange) -> Diagnostic {
    Diagnostic {
        code: "DOC008",
        severity: Severity::Warning,
        message: format!(
            "parameter '{name}' is documented with a backslash in an ancestor whose docstring's \
             raw-string-ness (`r\"\"\"...\"\"\"` vs `\"\"\"...\"\"\"`) differs from this one's — \
             copying it verbatim would change what the backslash means, so it was left as-is; \
             resolve by matching raw-ness or documenting it locally with `override=`"
        ),
        range,
    }
}

/// The byte range spanning every existing `Parameters` entry, from the start of the first one
/// (in source order) through the end of the last, plus the shared indentation to re-anchor
/// every rebuilt line to (every entry in a section is required to sit at the same margin, so
/// any one of them gives the right answer for all of them). `None` when the section has no
/// entries at all yet — nothing to rebuild against — or when the entries aren't in well-formed,
/// non-overlapping text order (see below) — nothing safe to rebuild against.
///
/// The rebuild this anchors assumes `entries`' iteration (insertion) order matches its ranges'
/// text order, since it uses index-adjacency to decide which original gaps to preserve. That
/// assumption can break on real-world, non-numpydoc-conformant input: a docstring section header
/// the parser doesn't recognize (e.g. `Example` where only the plural `Examples` is canonical)
/// leaves everything after it parsed as part of the *previous* recognized section's body, so
/// unrelated prose gets mis-parsed as bogus "parameter" entries — and if two of those bogus
/// entries happen to collide on the same literal name (e.g. two `.. code-block:: python`
/// directives), `IndexMap::insert` overwrites the earlier one's *range* in place without moving
/// its *position*, leaving that index pointing at a much-later span while a still-earlier index
/// points earlier in the text. Rather than try to rebuild against that corrupted bookkeeping
/// (previously an observed panic against real SimPEG source), bail out and leave the docstring
/// untouched — the same "don't touch what wasn't understood" stance the parser already takes
/// elsewhere (e.g. DOC008's backslash-skip).
///
/// One legitimate case shares an identical range across several *consecutive* entries on
/// purpose: `numpydoc`'s `name1, name2 : shared type` syntax documents multiple parameters with
/// one entry, so `parse_section` inserts the same `range` under every comma-separated name in a
/// tight run. That's a tie, not an inversion — allowed here explicitly (checked before the
/// inversion test) — real SimPEG source with `alpha_x, alpha_y, alpha_z : ...` was seen tripping
/// the inversion check and bailing out (surfacing as spurious `DOC010`s for unrelated inherited
/// parameters elsewhere in the same docstring) before this carve-out existed.
fn primary_block_span(entries: &IndexMap<String, ParamEntry>, docstring_text: &str) -> Option<(TextRange, String)> {
    let first = entries.values().next()?;
    let last = entries.values().last()?;
    let mut prev: Option<&ParamEntry> = None;
    for entry in entries.values() {
        if let Some(prev) = prev {
            if entry.range != prev.range && entry.range.start() < prev.range.end() {
                return None;
            }
        }
        prev = Some(entry);
    }
    let indent = line_indent_before(docstring_text, usize::from(first.range.start()));
    Some((TextRange::new(first.range.start(), last.range.end()), indent))
}

/// When rebuilding a `Parameters` block in signature order, two consecutive entries that were
/// *already* directly next to each other, in the same order, on disk get to keep whatever text
/// originally separated them (including any blank line an author put there) instead of the
/// tool's own plain `"\n{indent}"` separator. `None` whenever the pair is actually being
/// reordered around — either name is new, or they weren't neighbors before — in which case the
/// caller falls back to the default separator, since there's no original gap to preserve.
fn original_gap_if_still_adjacent(
    prev_name: &str,
    next_name: &str,
    entries: &IndexMap<String, ParamEntry>,
    docstring_text: &str,
) -> Option<String> {
    let prev_index = entries.get_index_of(prev_name)?;
    let next_index = entries.get_index_of(next_name)?;
    if next_index != prev_index + 1 {
        return None;
    }
    let (_, prev_entry) = entries.get_index(prev_index)?;
    let (_, next_entry) = entries.get_index(next_index)?;
    let start = usize::from(prev_entry.range.end());
    let end = usize::from(next_entry.range.start());
    if start > end {
        // Defensive only: `primary_block_span`'s own well-formedness check already keeps the
        // caller from reaching this function with a corrupted (non-monotonic) `entries` map, but
        // this helper shouldn't assume it'll only ever be called from there.
        return None;
    }
    Some(docstring_text[start..end].to_string())
}

/// When `merge_shared_parameters` is on, groups consecutive names in `names` that are all
/// auto-managed (not overridden, and documented by some ancestor) and share identical
/// documentation — type, description, *and* origin — into one run, so the caller can render them
/// back out as a single `nameA, nameB : shared type` line instead of duplicating the same text
/// once per name. This mirrors `numpydoc`'s own convention for exactly this, and how the group
/// was almost certainly documented by whichever ancestor originally authored it. Every other name
/// — authored, overridden, a run that doesn't match, or grouping simply disabled — is its own
/// singleton group. Requiring the *origin* to match too (not just the rendered text) keeps a
/// merged group meaningful for provenance purposes; two coincidentally-identical descriptions from
/// different ancestors are never merged.
fn group_names_for_rendering<'a>(
    names: &'a [String],
    effective_overrides: &HashSet<String>,
    inherited_from_ancestors: &IndexMap<String, ParamEntry>,
    merge_shared_parameters: bool,
) -> Vec<Vec<&'a String>> {
    let mut groups: Vec<Vec<&'a String>> = Vec::new();
    let mut last_entry: Option<&ParamEntry> = None;
    for name in names {
        let entry = if merge_shared_parameters && !effective_overrides.contains(name) {
            inherited_from_ancestors.get(name)
        } else {
            None
        };
        let extends_last = match (entry, last_entry) {
            (Some(e), Some(prev)) => {
                e.type_description == prev.type_description && e.description == prev.description && e.origin == prev.origin
            }
            _ => false,
        };
        if extends_last {
            groups.last_mut().expect("extends_last implies a prior group exists").push(name);
        } else {
            groups.push(vec![name]);
        }
        last_entry = entry;
    }
    groups
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
    target_is_raw: bool,
    doc_range: TextRange,
    style: &NumpydocStyle,
    provenance_mode: ProvenanceMode,
    provenance_entries: &mut Vec<ProvenanceEntry>,
    edits: &mut Vec<TextEdit>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    use crate::style::ParamSectionKind;

    let target_entries = match target {
        ParamSectionKind::Primary => &parsed_entries.primary,
        ParamSectionKind::Secondary => &parsed_entries.secondary,
    };

    let newline = newline_style(docstring_text);
    let named: HashSet<&str> = signature_names.iter().map(String::as_str).collect();
    let mut pending_inserts: Vec<String> = Vec::new();

    // When `expand_kwargs=` targets `Parameters`, remove any expand_kwargs-eligible entry left
    // behind in `Other Parameters` from a prior run under the *other* mode -- otherwise switching
    // modes would leave the same parameter documented in both sections at once. The reverse
    // direction (a stale entry left in `Parameters` when the target switches away from it) is
    // handled by the caller's own whole-block rebuild instead, for free, by simply excluding that
    // name from what it rebuilds -- see its own comment for why that's the right split: only
    // `Other Parameters` has no other mechanism ever managing it, so only this direction needs an
    // explicit removal edit here.
    if target == ParamSectionKind::Primary {
        let stale_in_secondary: Vec<String> = parsed_entries
            .secondary
            .keys()
            .filter(|name| {
                inherited_from_ancestors.contains_key(name.as_str())
                    && !named.contains(name.as_str())
                    && !effective_overrides.contains(name.as_str())
                    && !effective_exclude.contains(name.as_str())
            })
            .cloned()
            .collect();
        if !stale_in_secondary.is_empty() {
            let keep: Vec<&String> =
                parsed_entries.secondary.keys().filter(|n| !stale_in_secondary.contains(*n)).collect();
            if keep.is_empty() && parsed_entries.secondary_trailing_var_args.is_none() {
                // Every entry in `Other Parameters` was ours, and there's no hand-written
                // trailing `*args`/`**kwargs` line to preserve -- the whole section (header
                // included) is now dead weight; remove it and the blank-line gap that separated
                // it from whatever precedes it, reconnecting cleanly.
                //
                // When a trailing var-args line IS present, falling through to the "some entries
                // survive" branch below does the right thing for free: `keep` is empty, so
                // `rebuilt` is the empty-string join of zero entries, and the span (computed from
                // real `secondary` entries only, same as always) gets replaced with `""` --
                // removing exactly the stale entries while leaving the header before them and the
                // var-args line after them untouched, rather than orphaning it below wherever the
                // header used to be.
                if let Some(header_start) = parsed_entries.secondary_header_start {
                    let last =
                        parsed_entries.secondary.values().max_by_key(|e| e.range.end()).expect("secondary is non-empty here");
                    let from = end_of_content_before(docstring_text, header_start);
                    let delete_range = TextRange::new(TextSize::try_from(from).unwrap(), last.range.end());
                    edits.push(TextEdit::new(offset_range(delete_range, inner_range_start), String::new()));
                }
            } else {
                // Some entries in `Other Parameters` are unrelated to this -- keep the section
                // and its header, rebuilding just the entries span from the survivors' own
                // verbatim on-disk text (dropping the stale ones), same as the primary block
                // rebuild's own "diff the rebuilt span against what's on disk" pattern.
                let first =
                    parsed_entries.secondary.values().min_by_key(|e| e.range.start()).expect("secondary is non-empty here");
                let last =
                    parsed_entries.secondary.values().max_by_key(|e| e.range.end()).expect("secondary is non-empty here");
                let indent = line_indent_before(docstring_text, usize::from(first.range.start()));
                let rebuilt: String = keep
                    .iter()
                    .map(|name| {
                        let entry = parsed_entries.secondary.get(name.as_str()).expect("came from these same keys");
                        docstring_text[usize::from(entry.range.start())..usize::from(entry.range.end())].to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(&format!("{newline}{indent}"));
                let span = TextRange::new(first.range.start(), last.range.end());
                let current = &docstring_text[usize::from(span.start())..usize::from(span.end())];
                if current != rebuilt {
                    edits.push(TextEdit::new(offset_range(span, inner_range_start), rebuilt));
                }
            }
        }
    }

    for (name, ancestor_entry) in inherited_from_ancestors.iter() {
        if named.contains(name.as_str()) || effective_overrides.contains(name) || effective_exclude.contains(name) {
            continue;
        }
        if target == ParamSectionKind::Primary && parsed_entries.primary.contains_key(name) {
            // Already documented in `Parameters` -- the caller's own whole-block rebuild already
            // regenerates it as an ordinary auto-managed entry (via the signature/stray-extras
            // walk, unfiltered for target `Primary`), including its own provenance tracking.
            // Handling it again here would emit a second edit for the same byte range the
            // whole-block rebuild's own edit already covers -- `apply_edits` treats overlapping
            // edits as a bug, not something to merge.
            continue;
        }
        let candidate = style.format_entry(ancestor_entry, "", newline);
        if !safe_to_copy_across_raw_ness(ancestor_entry.is_raw, target_is_raw, &candidate) {
            diagnostics.push(backslash_raw_mismatch_diagnostic(name, doc_range));
            continue;
        }
        if let Some(origin) = &ancestor_entry.origin {
            provenance_entries.push(ProvenanceEntry {
                name: name.clone(),
                origin: origin.clone(),
            });
        }
        let new_text = provenance::maybe_append_inline_note(
            candidate,
            &parsed_entries.margin_indent,
            newline,
            provenance_mode,
            ancestor_entry.origin.as_ref(),
        );
        match target_entries.get(name) {
            Some(existing) => {
                // Compare the fully rendered candidate against the raw on-disk slice, not
                // `existing`'s structural fields (`type_description`/`description`) against
                // `ancestor_entry`'s -- inline mode can bake a provenance note into on-disk text
                // that never appears in `ancestor_entry.description` itself, so a structural
                // comparison would wrongly see permanent drift and re-emit the same edit forever.
                // This is also just more directly correct in general: it's exactly what the
                // primary/secondary block-rebuild path already does (rebuild, then diff against
                // `current_block`), so `expand_kwargs` now matches that same pattern.
                let current = &docstring_text[usize::from(existing.range.start())..usize::from(existing.range.end())];
                if current != new_text {
                    edits.push(TextEdit::new(offset_range(existing.range, inner_range_start), new_text));
                }
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
        ParamSectionKind::Secondary => {
            append_point(&parsed_entries.primary, parsed_entries.primary_trailing_var_args, docstring_text)
        }
    };
    let target_trailing_var_args = match target {
        ParamSectionKind::Primary => parsed_entries.primary_trailing_var_args,
        ParamSectionKind::Secondary => parsed_entries.secondary_trailing_var_args,
    };
    let (anchor, indent, header) = if let Some((insertion, indent)) =
        append_point(target_entries, target_trailing_var_args, docstring_text)
    {
        (insertion, indent, String::new())
    } else if let Some((insertion, indent)) = fallback_anchor {
        let raw_header = style.synthesize_section(target);
        let indented_header: String =
            raw_header.lines().map(|line| format!("{indent}{line}{newline}")).collect();
        // A blank line ahead of the freshly-synthesized section header, separating it from the
        // `Parameters` section's own last entry immediately above -- matches numpydoc convention
        // (a section boundary is always blank-line-separated from whatever precedes it) and the
        // same convention `insert_missing_sections` already follows for a brand-new `Parameters`
        // section.
        (insertion, indent, format!("{newline}{newline}{indented_header}"))
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
            text.push_str(newline);
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

    // `provenance_mode` off: this helper backs every pre-existing test in this module, almost
    // all written before provenance annotations existed and asserting exact docstring text that
    // has nothing to do with them -- defaulting to `Comment` here would bolt an unrelated
    // comment block onto any of them that happens to exercise inheritance (most of them).
    // Provenance's own behavior is exercised via `sync_with_provenance_mode` instead.
    fn sync(source: &str) -> (String, Vec<Diagnostic>) {
        let options = SyncOptions {
            project_default_style: None,
            insert_missing_sections: false,
            provenance_mode: ProvenanceMode::Off,
            merge_shared_parameters: false,
        };
        sync_source_with_options(source, options).expect("fixture must parse")
    }

    fn sync_with_provenance_mode(source: &str, provenance_mode: ProvenanceMode) -> (String, Vec<Diagnostic>) {
        let options = SyncOptions {
            project_default_style: None,
            insert_missing_sections: false,
            provenance_mode,
            merge_shared_parameters: false,
        };
        sync_source_with_options(source, options).expect("fixture must parse")
    }

    fn sync_inserting_missing_sections(source: &str) -> (String, Vec<Diagnostic>) {
        let options = SyncOptions {
            project_default_style: None,
            insert_missing_sections: true,
            // Off here so this helper's pre-existing exact-text assertions (written before
            // provenance annotations existed) aren't affected by an unrelated feature.
            provenance_mode: ProvenanceMode::Off,
            merge_shared_parameters: false,
        };
        sync_source_with_options(source, options).expect("fixture must parse")
    }

    fn sync_multi(files: &[(&str, &str)]) -> HashMap<String, FileOutput> {
        let owned: Vec<(PathBuf, String)> = files.iter().map(|(p, t)| (PathBuf::from(p), t.to_string())).collect();
        // Same rationale as `sync`'s own comment: `provenance_mode` off to keep this pre-existing
        // helper's assertions unaffected by the new, unrelated default.
        let options = SyncOptions {
            project_default_style: None,
            insert_missing_sections: false,
            provenance_mode: ProvenanceMode::Off,
            merge_shared_parameters: false,
        };
        sync_project_with_options(&owned, options)
            .into_iter()
            .map(|out| (out.path.to_string_lossy().replace('\\', "/"), out))
            .collect()
    }

    fn sync_merging_shared_parameters(source: &str) -> (String, Vec<Diagnostic>) {
        let options = SyncOptions {
            project_default_style: None,
            insert_missing_sections: false,
            provenance_mode: ProvenanceMode::Off,
            merge_shared_parameters: true,
        };
        sync_source_with_options(source, options).expect("fixture must parse")
    }

    // `sync_source`'s test-only synthetic path is always `<source>.py`, so its module name is
    // always literally `<source>` -- embedded verbatim in the exact-text assertions below.
    const MULTI_ANCESTOR_PROVENANCE_PY: &str = "\
class BaseSrc:
    \"\"\"BaseSrc.

    Parameters
    ----------
    location : (3,) array_like
        Source location.
    receiver_list : list of BaseRx
        Receivers.
    \"\"\"

    def __init__(self, location, receiver_list):
        pass


class BaseFDEMSrc(BaseSrc):
    \"\"\"BaseFDEMSrc.

    Parameters
    ----------
    location : (3,) array_like
        Source location.
    receiver_list : list of BaseRx
        Receivers.
    frequency : float
        Source frequency.
    \"\"\"

    def __init__(self, location, receiver_list, frequency):
        pass


class MagDipole(BaseFDEMSrc):
    \"\"\"MagDipole.

    Parameters
    ----------
    location : (3,) array_like
        stale
    receiver_list : list of BaseRx
        stale
    frequency : float
        stale
    \"\"\"

    def __init__(self, location, receiver_list, frequency):
        pass
";

    #[test]
    fn comment_mode_summarizes_two_distinct_ancestor_origins_grouped_and_ordered_by_signature() {
        let (output, diagnostics) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Comment);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let mag_dipole = &output[output.find("class MagDipole").unwrap()..];
        let expected = "\
class MagDipole(BaseFDEMSrc):
    \"\"\"MagDipole.

    Parameters
    ----------
    location : (3,) array_like
        Source location.
    receiver_list : list of BaseRx
        Receivers.
    frequency : float
        Source frequency.
    \"\"\"
    # docerator: provenance
    # docerator: from <source>.BaseSrc: location, receiver_list
    # docerator: from <source>.BaseFDEMSrc: frequency

    def __init__(self, location, receiver_list, frequency):
        pass
";
        assert_eq!(mag_dipole, expected);
    }

    #[test]
    fn comment_mode_is_a_noop_on_a_second_run_with_no_underlying_changes() {
        let (first, diagnostics) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Comment);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let (second, diagnostics) = sync_with_provenance_mode(&first, ProvenanceMode::Comment);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert_eq!(first, second, "a second run must not change anything further");
    }

    #[test]
    fn provenance_off_produces_no_block() {
        let (output, diagnostics) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Off);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert!(!output.contains("docerator: provenance"));
    }

    #[test]
    fn switching_from_comment_to_off_deletes_a_previously_inserted_block() {
        let (with_comment, _) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Comment);
        assert!(with_comment.contains("docerator: provenance"));
        let (after_off, diagnostics) = sync_with_provenance_mode(&with_comment, ProvenanceMode::Off);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert!(!after_off.contains("docerator: provenance"));
        let (direct_off, _) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Off);
        assert_eq!(after_off, direct_off, "switching to Off must converge to the same text as a from-scratch Off run");
    }

    #[test]
    fn switching_from_comment_to_inline_deletes_the_comment_block_and_adds_inline_notes() {
        let (with_comment, _) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Comment);
        let (after_inline, diagnostics) = sync_with_provenance_mode(&with_comment, ProvenanceMode::Inline);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert!(!after_inline.contains("docerator: provenance"));
        assert!(after_inline.contains("(Inherited from <source>.BaseSrc.)"));
        assert!(after_inline.contains("(Inherited from <source>.BaseFDEMSrc.)"));
    }

    #[test]
    fn switching_from_inline_to_comment_strips_the_baked_in_note_from_disk() {
        let (with_inline, _) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Inline);
        assert!(with_inline.contains("(Inherited from"));
        let (after_comment, diagnostics) = sync_with_provenance_mode(&with_inline, ProvenanceMode::Comment);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert!(!after_comment.contains("(Inherited from"));
        let (direct_comment, _) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Comment);
        assert_eq!(after_comment, direct_comment, "switching to Comment must converge to the same text as a from-scratch Comment run");
    }

    #[test]
    fn inline_mode_is_idempotent_on_rerun() {
        let (first, diagnostics) = sync_with_provenance_mode(MULTI_ANCESTOR_PROVENANCE_PY, ProvenanceMode::Inline);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let (second, diagnostics) = sync_with_provenance_mode(&first, ProvenanceMode::Inline);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert_eq!(first, second, "a second run must not change anything further");
    }

    #[test]
    fn override_and_locally_authored_entries_never_receive_provenance() {
        let source = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    arg1 : int
        Base doc.
    \"\"\"

    def __init__(self, arg1):
        pass


# docerator: override=arg1
class Child(Base):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Child's own, locally authored text.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync_with_provenance_mode(source, ProvenanceMode::Comment);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(!child.contains("docerator: provenance"), "an overridden entry must never appear in a provenance block");

        let (inline_output, diagnostics) = sync_with_provenance_mode(source, ProvenanceMode::Inline);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert!(!inline_output.contains("Inherited from"), "an overridden entry must never receive an inline note");
    }

    #[test]
    fn an_entry_withheld_for_raw_ness_mismatch_gets_no_provenance_note() {
        let source = "\
class Base:
    r\"\"\"Base.

    Parameters
    ----------
    arg1 : int
        Uses a backslash: \\alpha.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Base):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should NOT resync -- raw-ness mismatch makes it unsafe.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync_with_provenance_mode(source, ProvenanceMode::Comment);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC008");
        assert!(!output.contains("docerator: provenance"), "an entry withheld for raw-ness safety must not appear in a provenance block");
    }

    const EXPAND_KWARGS_PROVENANCE_PY: &str = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    extra1 : int
        Extra1 doc.
    extra2 : str
        Extra2 doc.
    \"\"\"

    def __init__(self, extra1=1, extra2=\"x\"):
        pass


# docerator: expand_kwargs
class Child(Base):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.

    Other Parameters
    ----------------
    extra1 : int
        stale
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";

    #[test]
    fn inline_mode_appends_a_note_to_expand_kwargs_targets_both_regenerated_and_newly_inserted() {
        let (output, diagnostics) = sync_with_provenance_mode(EXPAND_KWARGS_PROVENANCE_PY, ProvenanceMode::Inline);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(
            child.contains("extra1 : int\n        Extra1 doc.\n        (Inherited from <source>.Base.)"),
            "regenerated existing entry should get a note:\n{child}"
        );
        assert!(
            child.contains("extra2 : str\n        Extra2 doc.\n        (Inherited from <source>.Base.)"),
            "newly-inserted entry should get a note:\n{child}"
        );
    }

    #[test]
    fn comment_mode_covers_expand_kwargs_targets_too() {
        let (output, diagnostics) = sync_with_provenance_mode(EXPAND_KWARGS_PROVENANCE_PY, ProvenanceMode::Comment);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(child.contains("# docerator: from <source>.Base: extra1, extra2"), "got:\n{child}");
    }

    #[test]
    fn expand_kwargs_in_sync_check_compares_rendered_text_not_structural_fields() {
        // Regression test for the fix this feature required: `extra1`'s structural fields
        // (`type_description`/`description`) match the ancestor's exactly, but under Inline mode
        // the on-disk text (from a prior Comment-mode run, with no note baked in) differs from
        // what Inline mode would render (which does have a note) -- an edit must still be
        // emitted, not skipped as "already in sync" by a stale structural-only comparison.
        let (comment_output, _) = sync_with_provenance_mode(EXPAND_KWARGS_PROVENANCE_PY, ProvenanceMode::Comment);
        let (inline_output, diagnostics) = sync_with_provenance_mode(&comment_output, ProvenanceMode::Inline);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &inline_output[inline_output.find("class Child").unwrap()..];
        assert!(
            child.contains("extra1 : int\n        Extra1 doc.\n        (Inherited from <source>.Base.)"),
            "expected the inline note to actually be applied on this run, got:\n{child}"
        );
    }

    #[test]
    fn hand_written_content_right_after_the_docstring_blocks_provenance_comment_insertion() {
        // Child's own `arg1` text already matches Base's exactly, so the Parameters section
        // itself needs no edit at all -- isolating this test to *only* the blocked comment
        // insertion, so `output == source` cleanly proves nothing else was touched either.
        let source = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    arg1 : int
        Base doc.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Base):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Base doc.
    \"\"\"; real_code_on_the_same_line = 1

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync_with_provenance_mode(source, ProvenanceMode::Comment);
        assert_eq!(output, source, "must never risk corrupting a semicolon-chained statement");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC013");
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
    arg2 : str
        The second argument, also from Parent.
    extra : bool
        This one is genuinely new to Child and must be left alone.
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
    fn expand_kwargs_synthesizes_other_parameters_after_an_existing_trailing_kwargs_line() {
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

    def __init__(self, arg1, extra1, **kwargs):
        pass


# docerator: expand_kwargs
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    **kwargs
        Forwarded to :py:class:`.Parent`.
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

    def __init__(self, arg1, extra1, **kwargs):
        pass


# docerator: expand_kwargs
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    **kwargs
        Forwarded to :py:class:`.Parent`.

    Other Parameters
    ----------------
    extra1 : bool
        Extra1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        assert_eq!(output, expected);
    }

    #[test]
    fn expand_kwargs_appends_to_existing_other_parameters_section_past_its_own_trailing_kwargs_line() {
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

    def __init__(self, arg1, extra1, extra2, **kwargs):
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
    **kwargs
        Forwarded to :py:class:`.Parent`.
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

    def __init__(self, arg1, extra1, extra2, **kwargs):
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
    **kwargs
        Forwarded to :py:class:`.Parent`.
    extra2 : float
        Extra2 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        assert_eq!(output, expected);
    }

    #[test]
    fn expand_kwargs_into_parameters_appends_past_an_existing_trailing_kwargs_line() {
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

    def __init__(self, arg1, extra1, **kwargs):
        pass


# docerator: expand_kwargs=parameters
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    **kwargs
        Forwarded to :py:class:`.Parent`.
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

    def __init__(self, arg1, extra1, **kwargs):
        pass


# docerator: expand_kwargs=parameters
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    **kwargs
        Forwarded to :py:class:`.Parent`.
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
    fn expand_kwargs_mode_switch_to_parameters_does_not_orphan_a_hand_written_kwargs_in_other_parameters() {
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

    def __init__(self, arg1, extra1, **kwargs):
        pass


# docerator: expand_kwargs=parameters
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
    **kwargs
        Forwarded to :py:class:`.Parent`.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        // the stale `Other Parameters` copy of extra1 is gone (it now lives in Parameters)...
        assert_eq!(child.matches("extra1").count(), 1);
        // ...but the section header and the hand-written kwargs line both survive intact.
        assert!(child.contains("Other Parameters"));
        assert!(child.contains("**kwargs\n        Forwarded to :py:class:`.Parent`."));
    }

    #[test]
    fn primary_block_rebuild_never_touches_a_trailing_kwargs_line() {
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

    def __init__(self, arg1, extra1, **kwargs):
        pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc, stale text that needs resyncing.
    **kwargs
        Forwarded to :py:class:`.Parent`.
    \"\"\"

    def __init__(self, arg1, extra1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(child.contains("arg1 : int\n        Arg1 doc."));
        assert!(!child.contains("stale text"));
        // the newly-inserted extra1 lands before **kwargs, which stays last, untouched.
        let extra1_pos = child.find("extra1").unwrap();
        let kwargs_pos = child.find("**kwargs").unwrap();
        assert!(extra1_pos < kwargs_pos);
        assert!(child.contains("**kwargs\n        Forwarded to :py:class:`.Parent`."));
    }

    #[test]
    fn expand_kwargs_past_trailing_kwargs_is_idempotent_on_rerun() {
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

    def __init__(self, arg1, extra1, **kwargs):
        pass


# docerator: expand_kwargs
class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    **kwargs
        Forwarded to :py:class:`.Parent`.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (first_pass, _) = sync(source);
        let (second_pass, diagnostics) = sync(&first_pass);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert_eq!(first_pass, second_pass);
    }

    #[test]
    fn synthesizing_other_parameters_from_scratch_leaves_a_blank_line_before_it() {
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
        let child = &output[output.find("class Child").unwrap()..];
        assert!(
            child.contains("arg1 : int\n        Arg1 doc.\n\n    Other Parameters\n    ----------------\n"),
            "got:\n{child}"
        );
    }

    #[test]
    fn switching_expand_kwargs_from_other_to_parameters_removes_the_whole_other_section_when_emptied() {
        // Child's docstring already has `extra1` documented in `Other Parameters` (simulating a
        // prior run under bare `expand_kwargs`), but the directive now targets `Parameters`
        // instead -- `extra1` must move, and since it was the *only* entry in `Other Parameters`,
        // that whole section (header included) must be removed, not left behind empty.
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

    Other Parameters
    ----------------
    extra1 : bool
        Extra1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(!child.contains("Other Parameters"), "got:\n{child}");
        assert_eq!(child.matches("extra1").count(), 1, "extra1 must be documented exactly once:\n{child}");
        assert!(child.contains("arg1 : int\n        Arg1 doc.\n    extra1 : bool\n        Extra1 doc.\n    \"\"\""), "got:\n{child}");
    }

    #[test]
    fn switching_expand_kwargs_from_other_to_parameters_keeps_unrelated_other_parameters_entries() {
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

    Other Parameters
    ----------------
    extra1 : bool
        Extra1 doc.
    unrelated : str
        Not from expand_kwargs at all.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert_eq!(child.matches("extra1").count(), 1, "extra1 must be documented exactly once:\n{child}");
        assert!(
            child.contains("Other Parameters\n    ----------------\n    unrelated : str\n        Not from expand_kwargs at all."),
            "unrelated entry must survive in Other Parameters:\n{child}"
        );
    }

    #[test]
    fn switching_expand_kwargs_from_parameters_to_other_does_not_leave_a_stray_copy_behind() {
        // Child's docstring already has `extra1` documented in `Parameters` (simulating a prior
        // run under `expand_kwargs=parameters`), but the directive is now bare `expand_kwargs`
        // (targeting `Other Parameters` instead) -- `extra1` must move, not end up in both places.
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


# docerator: expand_kwargs
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
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert_eq!(child.matches("extra1").count(), 1, "extra1 must be documented exactly once:\n{child}");
        assert!(child.contains("Other Parameters"), "got:\n{child}");
        let parameters_section = &child[..child.find("Other Parameters").unwrap()];
        assert!(!parameters_section.contains("extra1"), "extra1 must not remain in Parameters:\n{child}");
    }

    #[test]
    fn switching_expand_kwargs_modes_is_idempotent_on_rerun() {
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

    Other Parameters
    ----------------
    extra1 : bool
        Extra1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (first_pass, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let (second_pass, diagnostics) = sync(&first_pass);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert_eq!(first_pass, second_pass, "a second run must not change anything further");
    }

    #[test]
    fn stale_entry_already_present_under_target_parameters_resyncs_without_an_overlapping_edit_panic() {
        // A latent, pre-existing correctness gap this same fix closes: with `expand_kwargs=
        // parameters` targeting an entry that's *already* documented (with stale text) in
        // `Parameters`, the whole-block rebuild and `expand_kwargs`'s own per-entry update used
        // to both try to edit the exact same byte range -- `apply_edits` panics on overlapping
        // edits. `expand_kwargs` must defer entirely to the whole-block rebuild here.
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    extra1 : bool
        Extra1 doc, UPDATED.
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
        Stale extra1 doc.
    \"\"\"

    def __init__(self, arg1, **kwargs):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(child.contains("extra1 : bool\n        Extra1 doc, UPDATED."), "got:\n{child}");
        assert!(!child.contains("Stale extra1 doc."), "got:\n{child}");
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
    fn resolves_base_class_through_attribute_access_on_a_name_imported_submodule() {
        // `from . import base` binds `base` via `ImportedSymbol::Name`, not `ImportedSymbol::
        // Module` -- Python's `from pkg import name` doesn't distinguish "name is a symbol
        // defined in pkg" from "name is itself one of pkg's submodules" at the syntax level, and
        // `base` here really is the sibling module `pkg/base.py`. Real-world shape this mirrors:
        // SimPEG's `frequency_domain/receivers.py` does `from ... import survey` then
        // `class BaseRx(survey.BaseRx):`.
        let child = child_using("from . import base");
        let child = child.replacen("class Child(Base):", "class Child(base.Base):", 1);
        let outputs = sync_multi(&[("pkg/__init__.py", ""), ("pkg/base.py", BASE_PY), ("pkg/child.py", &child)]);

        let child_out = &outputs["pkg/child.py"];
        assert!(child_out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", child_out.diagnostics);
        assert!(child_out.text.contains("Arg1 doc, from Base."));
        assert!(!child_out.text.contains("Stale text"));
    }

    #[test]
    fn resolves_base_class_through_attribute_access_on_a_name_imported_submodule_several_levels_up() {
        // Mirrors the exact real-world shape reported: a file nested several packages deep
        // climbs multiple levels (`from ... import survey`) to reach a top-level sibling module,
        // then uses attribute access on it for the base class.
        let child = child_using("from ... import base");
        let child = child.replacen("class Child(Base):", "class Child(base.Base):", 1);
        let outputs = sync_multi(&[
            ("pkg/__init__.py", ""),
            ("pkg/base.py", BASE_PY),
            ("pkg/sub/__init__.py", ""),
            ("pkg/sub/deeper/__init__.py", ""),
            ("pkg/sub/deeper/child.py", &child),
        ]);

        let child_out = &outputs["pkg/sub/deeper/child.py"];
        assert!(child_out.diagnostics.is_empty(), "unexpected diagnostics: {:?}", child_out.diagnostics);
        assert!(child_out.text.contains("Arg1 doc, from Base."));
    }

    #[test]
    fn resolves_parameters_from_multiple_base_classes() {
        // `resolve_and_rewrite` used to track only the *first* resolvable base ref, silently
        // ignoring every other one -- a class with true multiple inheritance only ever inherited
        // documentation from whichever base happened to be listed first.
        let source = "\
class BaseA:
    \"\"\"BaseA.

    Parameters
    ----------
    arg_a : int
        From BaseA.
    \"\"\"

    def __init__(self, arg_a=None):
        pass


class BaseB:
    \"\"\"BaseB.

    Parameters
    ----------
    arg_b : str
        From BaseB.
    \"\"\"

    def __init__(self, arg_b=None):
        pass


class Child(BaseA, BaseB):
    \"\"\"Child.

    Does some child-specific things.
    \"\"\"

    def __init__(self, arg_a=None, arg_b=None):
        pass
";
        let (output, diagnostics) = sync_inserting_missing_sections(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(child.contains("arg_a : int\n        From BaseA."), "got:\n{child}");
        assert!(child.contains("arg_b : str\n        From BaseB."), "got:\n{child}");
    }

    #[test]
    fn earlier_declared_base_wins_when_two_bases_document_the_same_parameter_differently() {
        // Approximates Python's real left-to-right MRO priority among direct bases without
        // implementing full C3 linearization (see `resolve_and_rewrite`'s own doc comment).
        let source = "\
class BaseA:
    \"\"\"BaseA.

    Parameters
    ----------
    arg : int
        From BaseA, should win.
    \"\"\"

    def __init__(self, arg=None):
        pass


class BaseB:
    \"\"\"BaseB.

    Parameters
    ----------
    arg : int
        From BaseB, should lose.
    \"\"\"

    def __init__(self, arg=None):
        pass


class Child(BaseA, BaseB):
    \"\"\"Child.

    Does some child-specific things.
    \"\"\"

    def __init__(self, arg=None):
        pass
";
        let (output, diagnostics) = sync_inserting_missing_sections(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(child.contains("arg : int\n        From BaseA, should win."), "got:\n{child}");
        assert!(!child.contains("From BaseB"), "got:\n{child}");
    }

    #[test]
    fn diamond_inheritance_shared_ancestor_contributes_once_correctly() {
        // BaseA and BaseB both reach CommonBase -- the shared ancestor's own entry must show up
        // exactly once, correctly, regardless of which of the two paths "wins" the merge (both
        // paths resolve the identical memoized CommonBase view, so there's nothing to conflict).
        let source = "\
class CommonBase:
    \"\"\"CommonBase.

    Parameters
    ----------
    common_arg : int
        From CommonBase.
    \"\"\"

    def __init__(self, common_arg=None):
        pass


class BaseA(CommonBase):
    \"\"\"BaseA.

    Parameters
    ----------
    arg_a : int
        From BaseA.
    \"\"\"

    def __init__(self, arg_a=None, common_arg=None):
        pass


class BaseB(CommonBase):
    \"\"\"BaseB.

    Parameters
    ----------
    arg_b : int
        From BaseB.
    \"\"\"

    def __init__(self, arg_b=None, common_arg=None):
        pass


class Child(BaseA, BaseB):
    \"\"\"Child.

    Does some child-specific things.
    \"\"\"

    def __init__(self, arg_a=None, arg_b=None, common_arg=None):
        pass
";
        let (output, diagnostics) = sync_inserting_missing_sections(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(child.contains("arg_a : int\n        From BaseA."), "got:\n{child}");
        assert!(child.contains("arg_b : int\n        From BaseB."), "got:\n{child}");
        assert_eq!(
            child.matches("common_arg : int\n        From CommonBase.").count(),
            1,
            "shared ancestor entry must appear exactly once:\n{child}"
        );
    }

    #[test]
    fn true_mro_prefers_a_more_specific_override_reached_through_a_non_overriding_branch() {
        // The textbook case C3 linearization exists to handle, and the exact scenario where a
        // simpler "merge each direct base's own already-flattened view in reverse declaration
        // order" scheme (this engine's own earlier, since-replaced implementation) gets the
        // wrong answer: `Grandparent` documents `x`; `BranchA(Grandparent)` redocuments it with
        // its own text (via `override=x` -- without that directive, a class's own inherited-but-
        // unoverridden text is just drift the tool resyncs back to the ancestor's, an unrelated,
        // pre-existing rule, so this scenario needs a genuine override to set up at all);
        // `BranchB(Grandparent)` never touches `x` (pure pass-through). `Child(BranchB, BranchA)`
        // -- note `BranchB` is declared FIRST. True MRO is `Child, BranchB, BranchA, Grandparent`:
        // `BranchB` doesn't define `x` itself, so resolution continues past it to `BranchA`,
        // whose own override wins -- even though `BranchB` was declared first. The old simplified
        // scheme couldn't tell "BranchB's `x` is really just Grandparent's, merely passed through"
        // from "BranchB's `x` is its own" (both look identical once flattened), so it incorrectly
        // let the first-declared `BranchB` win with Grandparent's stale text.
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    x : int
        From Grandparent, should lose.
    \"\"\"

    def __init__(self, x=None):
        pass



# docerator: override=x
class BranchA(Grandparent):
    \"\"\"BranchA.

    Parameters
    ----------
    x : int
        From BranchA, should win.
    \"\"\"

    def __init__(self, x=None):
        pass


class BranchB(Grandparent):
    \"\"\"BranchB.

    Does not redocument x at all -- pure pass-through from Grandparent.
    \"\"\"

    def __init__(self, x=None):
        pass


class Child(BranchB, BranchA):
    \"\"\"Child.

    Does some child-specific things.
    \"\"\"

    def __init__(self, x=None):
        pass
";
        let (output, diagnostics) = sync_inserting_missing_sections(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(child.contains("x : int\n        From BranchA, should win."), "got:\n{child}");
        assert!(!child.contains("From Grandparent"), "got:\n{child}");
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
    fn multiple_inheritance_still_resolves_correctly_when_every_ancestor_is_served_from_cache() {
        // `local_authored_memo` (the per-class "own contribution only" data the MRO merge reads)
        // is only ever populated by actually running a class's own per-method loop -- which a
        // cache hit skips entirely. Without also persisting it on disk (`CachedFile::
        // local_authored`) and restoring it into `local_authored_memo` on a cache hit, a
        // multiple-inheritance descendant reprocessed on a *later* run (while every one of its
        // ancestors stays fully cache-valid) would silently lose every cache-served ancestor's
        // contribution.
        let base_a = "\
class BaseA:
    \"\"\"BaseA.

    Parameters
    ----------
    arg_a : int
        From BaseA.
    \"\"\"

    def __init__(self, arg_a=None):
        pass
";
        let base_b = "\
class BaseB:
    \"\"\"BaseB.

    Parameters
    ----------
    arg_b : str
        From BaseB.
    \"\"\"

    def __init__(self, arg_b=None):
        pass
";
        let child_v1 = "\
from .base_a import BaseA
from .base_b import BaseB


class Child(BaseA, BaseB):
    \"\"\"Child.

    Parameters
    ----------
    arg_a : int
        Stale.
    arg_b : str
        Stale.
    \"\"\"

    def __init__(self, arg_a=None, arg_b=None):
        pass
";
        let files_v1 = owned_files(&[
            ("pkg/__init__.py", ""),
            ("pkg/base_a.py", base_a),
            ("pkg/base_b.py", base_b),
            ("pkg/child.py", child_v1),
        ]);
        let mut cache = Cache::default();
        let first = sync_project_with_cache(&files_v1, None, &mut cache);
        let first_child = text_for(&first, "pkg/child.py");
        assert!(first_child.contains("arg_a : int\n        From BaseA."), "got:\n{first_child}");
        assert!(first_child.contains("arg_b : str\n        From BaseB."), "got:\n{first_child}");

        // Round 2: child.py's content changes (to its own round-1 output -- already correctly
        // resynced, but a different byte sequence from round 1's original input, so its cache
        // entry is invalidated and it gets reprocessed); base_a.py/base_b.py are byte-identical
        // to round 1, so both stay fully cache-valid and get served from `memo`/
        // `local_authored_memo` rather than actually reprocessed.
        let files_v2 = owned_files(&[
            ("pkg/__init__.py", ""),
            ("pkg/base_a.py", base_a),
            ("pkg/base_b.py", base_b),
            ("pkg/child.py", first_child),
        ]);
        let second = sync_project_with_cache(&files_v2, None, &mut cache);
        let second_child = text_for(&second, "pkg/child.py");
        assert_eq!(second_child, first_child, "a reprocessed descendant must not lose a cache-served ancestor's contribution");
        assert!(second_child.contains("arg_a : int\n        From BaseA."), "got:\n{second_child}");
        assert!(second_child.contains("arg_b : str\n        From BaseB."), "got:\n{second_child}");
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

    #[test]
    fn method_view_passes_through_an_ancestor_that_does_not_override_it() {
        // Parent doesn't define __init__ at all -- Python's real MRO still reaches
        // Grandparent.__init__ transparently through it, so Child must too.
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc, from Grandparent.
    \"\"\"

    def __init__(self, arg1):
        pass


class Parent(Grandparent):
    pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should sync from Grandparent, through the non-overriding Parent.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert!(output.contains("Arg1 doc, from Grandparent."));
        assert!(!output.contains("Stale text that should sync"));
    }

    #[test]
    fn a_class_with_no_init_of_its_own_still_resyncs_its_own_class_level_docstring() {
        // Real-world regression (found against SimPEG's `TimeFields`/`Simulation3DElectricField`,
        // both `pass`-style -- no `__init__` of their own -- with a full `Parameters` section
        // documenting the inherited constructor anyway, by numpydoc convention). Without a
        // synthesized `__init__` entry, `class.methods` has nothing to iterate for this class at
        // all, so its own docstring was previously completely invisible to the whole engine --
        // never parsed, never checked, never resynced, forever.
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc, from Grandparent.
    \"\"\"

    def __init__(self, arg1):
        pass


class Parent(Grandparent):
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Stale text that should resync even though Parent has no __init__ of its own.
    \"\"\"
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let parent = &output[output.find("class Parent").unwrap()..];
        assert!(parent.contains("Arg1 doc, from Grandparent."), "got:\n{parent}");
        assert!(!parent.contains("Stale text"), "got:\n{parent}");
    }

    #[test]
    fn a_chain_of_init_less_classes_still_borrows_the_signature_from_further_up_the_mro() {
        // `Parent` has neither its own `__init__` nor its own docstring (nothing to synthesize
        // for it at all) -- `Middle`'s own synthesis must walk straight past it to `Grandparent`.
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc, from Grandparent.
    \"\"\"

    def __init__(self, arg1):
        pass


class Parent(Grandparent):
    pass


class Middle(Parent):
    \"\"\"Middle.

    Parameters
    ----------
    arg1 : int
        Stale text.
    \"\"\"
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let middle = &output[output.find("class Middle").unwrap()..];
        assert!(middle.contains("Arg1 doc, from Grandparent."), "got:\n{middle}");
        assert!(!middle.contains("Stale text"), "got:\n{middle}");
    }

    #[test]
    fn an_init_less_class_with_no_ancestor_defining_init_anywhere_is_left_untouched() {
        // No `__init__` anywhere in the chain to borrow a signature from -- nothing safe to
        // synthesize, so this docstring stays exactly as it was, same as before this feature.
        let source = "\
class Standalone:
    \"\"\"Standalone.

    Parameters
    ----------
    arg1 : int
        Some doc that looks like it should sync, but there is no ancestor __init__ to check it against.
    \"\"\"
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert_eq!(output, source);
    }

    #[test]
    fn class_level_skip_suppresses_the_synthesized_init_too() {
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc, from Grandparent.
    \"\"\"

    def __init__(self, arg1):
        pass


# docerator: skip
class Parent(Grandparent):
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Deliberately different text, must survive untouched because of skip.
    \"\"\"
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert_eq!(output, source);
    }

    #[test]
    fn an_init_less_class_missing_a_borrowed_parameter_gets_it_auto_inserted() {
        // Same "insert a missing-but-inherited entry into an already-existing `Parameters`
        // section" behavior any ordinary (non-synthesized) `__init__` already gets — the virtual
        // entry is processed by the exact same per-method logic, not a separate code path.
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    arg2 : str
        Arg2 doc.
    \"\"\"

    def __init__(self, arg1, arg2):
        pass


class Parent(Grandparent):
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    \"\"\"
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let parent = &output[output.find("class Parent").unwrap()..];
        assert!(parent.contains("arg2 : str\n        Arg2 doc."), "got:\n{parent}");
    }

    #[test]
    fn origin_survives_a_non_overriding_intermediate_ancestor() {
        // Same shape as the M8 pass-through fixture above, but inspecting `resolve_and_rewrite`'s
        // own returned `MethodViews` directly (rather than the synced text) to prove `arg1`'s
        // stamped origin names Grandparent -- the class that *really* authored it -- even though
        // it reaches Child by passing straight through the non-overriding Parent.
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc, from Grandparent.
    \"\"\"

    def __init__(self, arg1):
        pass


class Parent(Grandparent):
    pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should sync from Grandparent, through the non-overriding Parent.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let path = PathBuf::from("pkg_test.py");
        let files = [(path, source.to_string())];
        let mut diagnostics_by_file = HashMap::new();
        let project = project::build_project(&files, &mut diagnostics_by_file);
        let source_by_path: HashMap<&PathBuf, &str> = files.iter().map(|(p, t)| (p, t.as_str())).collect();
        let mut mro_memo = HashMap::new();
        let mut local_authored_memo = HashMap::new();
        let mut memo = HashMap::new();
        let mut edits_by_file = HashMap::new();
        let child_id = ClassId {
            module: "pkg_test".to_string(),
            name: "Child".to_string(),
        };
        let views = resolve_and_rewrite(
            &project,
            &child_id,
            None,
            false,
            ProvenanceMode::Off,
            false,
            &source_by_path,
            &mut mro_memo,
            &mut local_authored_memo,
            &mut memo,
            &mut edits_by_file,
            &mut diagnostics_by_file,
        );
        let arg1 = views.get("__init__").and_then(|m| m.get("arg1")).expect("arg1 entry in Child's resolved view");
        assert_eq!(
            arg1.origin.as_ref().map(EntryOrigin::display),
            Some("pkg_test.Grandparent".to_string())
        );
    }

    #[test]
    fn leading_blank_line_in_ancestor_description_is_reproduced_when_synced() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int

        Description with a blank line above it, as written by the author.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class Child").unwrap()..];
        assert!(child_section.contains("arg1 : int\n\n        Description with a blank line above it"));
    }

    #[test]
    fn trailing_blank_line_before_the_next_parameter_is_never_touched_by_a_splice() {
        // A blank line the author left between arg1's description and arg2's header, WITHIN
        // the same section (not the last-entry-before-next-section case), must survive a
        // splice of arg1 untouched -- it was never part of arg1's own range to begin with.
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Fresh text from Parent.
    arg2 : str
        Arg2 doc.
    \"\"\"

    def __init__(self, arg1, arg2):
        pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should sync.

    arg2 : str
        Arg2 doc.
    \"\"\"

    def __init__(self, arg1, arg2):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class Child").unwrap()..];
        // arg1 resynced, and the blank line the author left before arg2 is still there.
        assert!(child_section.contains("Fresh text from Parent.\n\n    arg2 : str"));
    }

    #[test]
    fn parameters_are_reordered_to_match_the_signature_and_a_pair_left_adjacent_keeps_its_original_gap() {
        // Docstring order is arg1, arg3, arg4, arg2 -- signature order is arg1, arg2, arg3, arg4.
        // arg3/arg4 are already neighbors, in the same order, both before and after the rebuild,
        // and the author left a blank line between them -- that gap must survive untouched. Every
        // other pair is newly adjacent because of the reorder and gets the tool's plain separator.
        let source = "\
class Standalone:
    \"\"\"Standalone.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    arg3 : str
        Arg3 doc.

    arg4 : float
        Arg4 doc.
    arg2 : bool
        Arg2 doc.
    \"\"\"

    def __init__(self, arg1, arg2, arg3, arg4):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

        let expected = "\
class Standalone:
    \"\"\"Standalone.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    arg2 : bool
        Arg2 doc.
    arg3 : str
        Arg3 doc.

    arg4 : float
        Arg4 doc.
    \"\"\"

    def __init__(self, arg1, arg2, arg3, arg4):
        pass
";
        assert_eq!(output, expected);
    }

    #[test]
    fn a_parameters_section_with_a_duplicate_documented_name_does_not_panic_and_is_left_untouched() {
        // Real-world regression (found against SimPEG source): a docstring section header the
        // parser doesn't recognize (only the plural "Examples" is canonical numpydoc, not
        // "Example") leaves everything after it parsed as bogus entries within the *previous*
        // section's body. If two of those bogus entries collide on the same literal name (e.g.
        // two `.. code-block:: python` directives), the second occurrence overwrites the first
        // one's *range* in the parsed map without moving its *position* -- breaking the
        // "insertion order matches text order" invariant the whole-block rebuild depends on.
        // This is a simplified repro of that same shape: `arg1` is documented twice, so its
        // entry ends up pointing at the SECOND (later) occurrence while still sitting at the
        // FIRST occurrence's position, ahead of `arg2` (whose own range is still the earlier,
        // in-between text). The rebuild must detect this and bail out rather than panic or
        // splice a corrupted block.
        let source = "\
class Standalone:
    \"\"\"Standalone.

    Parameters
    ----------
    arg1 : int
        First occurrence, stale text that would normally trigger a resync.
    arg2 : str
        Between the two arg1 occurrences.
    arg1 : bool
        Second occurrence of arg1, overwrites the first in the parsed map.
    \"\"\"

    def __init__(self, arg1, arg2):
        pass
";
        let (output, diagnostics) = sync(source);
        let _ = diagnostics;
        // Left completely untouched -- no panic, no corrupted splice.
        assert_eq!(output, source);
    }

    #[test]
    fn a_comma_grouped_shared_entry_does_not_trigger_a_spurious_bailout_or_get_duplicated() {
        // Real-world regression (found against SimPEG's `regularization/sparse.py`, `Sparse`
        // class): `numpydoc`'s `nameA, nameB : shared type` syntax gives every comma-separated
        // name the *exact same* on-disk range (`parse_section` inserts one `ParamEntry` per
        // name, but all pointing at the one shared line) -- `primary_block_span`'s "entries
        // aren't in monotonic text order" bail-out guard (added for the genuinely-malformed
        // duplicate-name case above) was treating that legitimate tie as an inversion too,
        // bailing out of the whole-block rebuild entirely. That turned a real SimPEG symptom
        // into "the docstring has no Parameters section to insert into" (`DOC010`) for an
        // unrelated, genuinely-insertable inherited parameter (`objfcts`) elsewhere in the same
        // class, even though the class plainly has a well-formed `Parameters` section.
        let source = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    extra : int
        Extra doc, from Base.
    \"\"\"

    def __init__(self, extra=None):
        pass


class Child(Base):
    \"\"\"Child.

    Parameters
    ----------
    alpha_x, alpha_y, alpha_z : float, optional
        Shared scaling constants.
    \"\"\"

    def __init__(self, alpha_x=1.0, alpha_y=1.0, alpha_z=1.0, extra=None):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        // The comma-group's shared text must appear exactly once, not once per name in the group
        // (the bug this guards: naively rebuilding per-name would triple it).
        assert_eq!(
            child.matches("alpha_x, alpha_y, alpha_z : float, optional").count(),
            1,
            "comma-group text duplicated:\n{child}"
        );
        // The genuinely inherited-but-locally-undocumented `extra` must now be correctly
        // inserted, not left as a DOC010 diagnostic-only gap.
        assert!(child.contains("extra : int\n        Extra doc, from Base."), "got:\n{child}");
    }

    #[test]
    fn a_trailing_example_singular_section_no_longer_blocks_the_whole_docstring_from_resyncing() {
        // Real-world regression (found against SimPEG's `electromagnetics/time_domain/fields.py`,
        // `FieldsTDEM` class): a `Parameters` section followed by an `Example` (singular, not the
        // canonical `Examples`) heading swallowed the entire rest of the docstring -- including a
        // second `.. code-block:: python` line colliding on the same synthesized "name" as an
        // earlier one -- as bogus entries, corrupting `parse_section`'s ordering invariant and
        // silently leaving the *whole* docstring untouched (no crash, no diagnostic -- just quietly
        // never resyncing `simulation`'s stale local text). Root-caused and fixed in
        // `numpydoc::find_sections` (an unrecognized-but-header-shaped line now still ends the
        // section before it); this is the end-to-end proof the actual reported symptom is gone.
        let source = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    simulation : simpeg.simulation.BaseTimeSimulation
        The simulation object used to compute the discrete field solution.
    \"\"\"

    def __init__(self, simulation):
        pass


class Child(Base):
    r\"\"\"Child.

    Parameters
    ----------
    simulation : simpeg.child.SomeSimulation
        Stale, locally-authored text that should resync from Base.

    Example
    -------
    Some prose.

    .. code-block:: python

        x = 1

    more prose

    .. code-block:: python

        y = 2
    \"\"\"

    def __init__(self, simulation):
        pass
";
        let (output, diagnostics) = sync(source);
        let codes: Vec<&str> = diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec!["DOC015"], "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(
            child.contains("simulation : simpeg.simulation.BaseTimeSimulation\n        The simulation object used to compute the discrete field solution."),
            "got:\n{child}"
        );
        assert!(!child.contains("Stale, locally-authored text"), "got:\n{child}");
    }

    const BASE_WITH_ALPHA_COMMA_GROUP_PY: &str = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    alpha_x, alpha_y, alpha_z : float or None, optional
        Scaling constants for the first order smoothness along x, y and z, respectively.
        If set to ``None``, the scaling constant is set automatically according to the
        value of the `length_scale` parameter.
    \"\"\"

    def __init__(self, alpha_x=None, alpha_y=None, alpha_z=None):
        pass
";

    fn child_inheriting_alpha_group(child_docstring: &str) -> String {
        format!(
            "{BASE_WITH_ALPHA_COMMA_GROUP_PY}\n\nclass Child(Base):\n    \"\"\"Child.\n\n\
             {child_docstring}    \"\"\"\n\n    def __init__(self, alpha_x=None, alpha_y=None, alpha_z=None):\n        pass\n"
        )
    }

    #[test]
    fn merge_shared_parameters_off_by_default_splits_inherited_comma_group_entries() {
        let source = child_inheriting_alpha_group("    Parameters\n    ----------\n    alpha_x : float\n        Stale.\n\n");
        let (output, diagnostics) = sync(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        let expected = "\
    Parameters
    ----------
    alpha_x : float or None, optional
        Scaling constants for the first order smoothness along x, y and z, respectively.
        If set to ``None``, the scaling constant is set automatically according to the
        value of the `length_scale` parameter.
    alpha_y : float or None, optional
        Scaling constants for the first order smoothness along x, y and z, respectively.
        If set to ``None``, the scaling constant is set automatically according to the
        value of the `length_scale` parameter.
    alpha_z : float or None, optional
        Scaling constants for the first order smoothness along x, y and z, respectively.
        If set to ``None``, the scaling constant is set automatically according to the
        value of the `length_scale` parameter.

    \"\"\"";
        assert!(child.contains(expected), "got:\n{child}");
    }

    #[test]
    fn merge_shared_parameters_on_recombines_identical_inherited_entries_into_one_line() {
        let source = child_inheriting_alpha_group("    Parameters\n    ----------\n    alpha_x : float\n        Stale.\n\n");
        let (output, diagnostics) = sync_merging_shared_parameters(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        let expected = "\
    Parameters
    ----------
    alpha_x, alpha_y, alpha_z : float or None, optional
        Scaling constants for the first order smoothness along x, y and z, respectively.
        If set to ``None``, the scaling constant is set automatically according to the
        value of the `length_scale` parameter.

    \"\"\"";
        assert!(child.contains(expected), "got:\n{child}");
    }

    #[test]
    fn merge_shared_parameters_is_idempotent_on_rerun() {
        let source = child_inheriting_alpha_group("    Parameters\n    ----------\n    alpha_x : float\n        Stale.\n\n");
        let (first_pass, diagnostics) = sync_merging_shared_parameters(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let (second_pass, diagnostics) = sync_merging_shared_parameters(&first_pass);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert_eq!(first_pass, second_pass, "a second run must not change anything further");
    }

    #[test]
    fn merge_shared_parameters_never_merges_entries_from_different_origins() {
        // Textually identical descriptions, but documented by two different ancestors --
        // merging them would misattribute one name's documentation to the wrong origin, so this
        // must never merge even with the option on. `arg1` reaches `Child` by pure pass-through
        // from `Grandparent` (never redocumented by `Parent`); `arg2` is `Parent`'s own.
        let source = "\
class Grandparent:
    \"\"\"Grandparent.

    Parameters
    ----------
    arg1 : float
        Shared-looking text.
    \"\"\"

    def __init__(self, arg1=None):
        pass


class Parent(Grandparent):
    \"\"\"Parent.

    Parameters
    ----------
    arg2 : float
        Shared-looking text.
    \"\"\"

    def __init__(self, arg1=None, arg2=None):
        pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : float
        Stale.
    \"\"\"

    def __init__(self, arg1=None, arg2=None):
        pass
";
        let (output, diagnostics) = sync_merging_shared_parameters(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        assert!(!child.contains("arg1, arg2"), "must not merge across different origins:\n{child}");
        assert!(child.contains("arg1 : float\n        Shared-looking text.\n    arg2 : float\n        Shared-looking text."), "got:\n{child}");
    }

    #[test]
    fn merge_shared_parameters_never_touches_locally_authored_or_overridden_entries() {
        let source = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    alpha_x, alpha_y, alpha_z : float or None, optional
        Scaling constants.
    \"\"\"

    def __init__(self, alpha_x=None, alpha_y=None, alpha_z=None):
        pass



# docerator: override=alpha_y
class Child(Base):
    \"\"\"Child.

    Parameters
    ----------
    alpha_x : float
        Placeholder.
    alpha_y : float
        Authored locally, must survive untouched.
    alpha_z : float
        Placeholder.
    \"\"\"

    def __init__(self, alpha_x=None, alpha_y=None, alpha_z=None):
        pass
";
        let (output, diagnostics) = sync_merging_shared_parameters(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child = &output[output.find("class Child").unwrap()..];
        // alpha_y is overridden (authored) -- it must never be folded into a merged group with
        // its ancestor-managed neighbors, even though its text happens to sit between them.
        assert!(child.contains("alpha_y : float\n        Authored locally, must survive untouched."), "got:\n{child}");
        assert!(!child.contains("alpha_x, alpha_y"), "an overridden entry must never be merged:\n{child}");
    }

    #[test]
    fn splicing_across_a_crlf_file_does_not_introduce_a_spurious_blank_line() {
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Fresh text from Parent.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should sync.
    \"\"\"

    def __init__(self, arg1):
        pass
"
        .replace('\n', "\r\n");

        let (output, diagnostics) = sync(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class Child").unwrap()..];
        assert!(child_section.contains("arg1 : int\r\n        Fresh text from Parent."));
        assert!(!child_section.contains("arg1 : int\r\n\r\n        Fresh text from Parent."));
        assert!(!child_section.contains("arg1 : int\n        Fresh text from Parent."));
    }

    #[test]
    fn reordering_a_crlf_file_uses_crlf_for_the_newly_synthesized_separator_too() {
        // Same reordering shape as `parameters_are_reordered_to_match_the_signature...`, but on
        // a CRLF file: arg1/arg2 are newly adjacent (no original gap to reuse), so the rebuild
        // must synthesize its own separator for that pair -- and it must match the rest of the
        // file's line endings, not silently downgrade to a bare `\n`.
        let source = "\
class Standalone:
    \"\"\"Standalone.

    Parameters
    ----------
    arg2 : bool
        Arg2 doc.
    arg1 : int
        Arg1 doc.
    \"\"\"

    def __init__(self, arg1, arg2):
        pass
"
        .replace('\n', "\r\n");

        let (output, diagnostics) = sync(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert!(
            output.contains("arg1 : int\r\n        Arg1 doc.\r\n    arg2 : bool\r\n        Arg2 doc."),
            "expected CRLF throughout the rebuilt block, got:\n{output}"
        );
        assert!(!output.contains("Arg1 doc.\n    arg2"), "found a bare LF separator in a CRLF file:\n{output}");
    }

    #[test]
    fn ancestor_regenerated_entry_in_a_crlf_file_uses_crlf_between_its_own_name_and_description() {
        // Real-world regression (found against SimPEG's `RawVec_e(BaseFDEMSrc)`): the entry
        // that's actually changing (`integrate`, regenerated from the ancestor's text) sits at
        // the END of an otherwise-unchanged, already-correctly-ordered block, so no reordering
        // or gap-preservation logic is even in play here -- this is purely about
        // `DocStyle::format_entry` itself, which used to hardcode a bare `\n` between the
        // `name : type` line it builds and the description it appends, regardless of what line
        // ending the rest of the (CRLF) file actually used.
        let source = "\
class BaseEMSrc:
    \"\"\"Base.

    Parameters
    ----------
    integrate : bool
        If ``True``, we integrate the source term
    \"\"\"

    def __init__(self, integrate=False, **kwargs):
        pass


class RawVec_e(BaseEMSrc):
    \"\"\"User-provided electric source term (s_e) class.

    Parameters
    ----------
    receiver_list : list of simpeg.survey.BaseRx objects
        Sets the receivers associated with the source
    frequency : float
        Source frequency
    s_e: numpy.ndarray
        Electric source term
    integrate : bool, default: ``False``
        If ``True``, integrate the source term; i.e. multiply by Me matrix
    \"\"\"

    def __init__(self, receiver_list, frequency, s_e, **kwargs):
        pass
"
        .replace('\n', "\r\n");
        let (output, diagnostics) = sync(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class RawVec_e").unwrap()..];
        assert!(
            child_section.contains("integrate : bool\r\n        If ``True``, we integrate the source term"),
            "expected CRLF between the regenerated name:type line and its description, got:\n{output}"
        );
        assert!(
            !child_section.contains("integrate : bool\n        If ``True``, we integrate the source term"),
            "found a bare LF separator in a CRLF file:\n{output}"
        );
    }

    #[test]
    fn a_raw_docstring_with_backslashes_only_in_prose_still_syncs_its_parameters_section() {
        // Real-world regression (found against SimPEG's `MagDipole`): a `r"""..."""` docstring
        // whose class-level prose is full of LaTeX (`\alpha`, `\mathbf{...}`, etc.) used to make
        // the ENTIRE docstring untouchable, because the old check was "does this docstring
        // contain a `\` anywhere" -- even though the Parameters section itself, and every entry
        // in it, is plain ASCII with no backslash at all. Docerator never decodes escapes (always
        // splices raw source bytes), so prose elsewhere in the same docstring was never actually
        // a hazard; only copying backslash-bearing text *across* a raw/non-raw boundary is.
        let source = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Fresh text from Parent.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Parent):
    r\"\"\"Child with LaTeX math elsewhere in the docstring.

    Uses :math:`\\alpha` and other backslash-heavy notation here, well before
    the Parameters section -- none of this involves any parameter's own text.

    Parameters
    ----------
    arg1 : int
        Stale text that should still resync.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class Child").unwrap()..];
        assert!(child_section.contains("arg1 : int\n        Fresh text from Parent."));
    }

    #[test]
    fn copying_a_backslash_between_docstrings_of_the_same_raw_ness_is_allowed() {
        // Both raw -- the same bytes mean the same thing in both literals, so there's nothing to
        // guard against even though the copied text contains a backslash.
        let source = "\
class Parent:
    r\"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Uses \\alpha in its description.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Parent):
    r\"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should resync.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class Child").unwrap()..];
        assert!(child_section.contains("arg1 : int\n        Uses \\alpha in its description."));
    }

    #[test]
    fn copying_a_backslash_across_a_raw_ness_mismatch_is_refused_with_a_diagnostic() {
        // Parent is raw (`\alpha` stays a literal backslash-a), Child is NOT raw (`\a` would be
        // interpreted as the bell-character escape at runtime) -- copying this text verbatim
        // would silently change what `Child.__init__.__doc__` actually contains. The entry must
        // be left exactly as authored, with a diagnostic explaining why it wasn't touched.
        let source = "\
class Parent:
    r\"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Uses \\alpha in its description.
    \"\"\"

    def __init__(self, arg1):
        pass


class Child(Parent):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should NOT resync -- raw-ness mismatch makes it unsafe.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        let (output, diagnostics) = sync(source);
        assert_eq!(output, source, "unsafe cross-raw-ness copy must leave the docstring untouched");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC008");
    }

    const PARENT_WITH_ARG1_PY: &str = "\
class Parent:
    \"\"\"Parent.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    \"\"\"

    def __init__(self, arg1):
        pass
";

    #[test]
    fn doc010_still_only_diagnoses_by_default_when_the_option_is_off() {
        let source = format!(
            "{PARENT_WITH_ARG1_PY}\n\nclass Child(Parent):\n    \"\"\"Child class.\n\n    Does some child-specific things.\n    \"\"\"\n\n    def __init__(self, arg1):\n        pass\n"
        );
        let (output, diagnostics) = sync(&source);
        assert_eq!(output, source, "opt-in off must leave the docstring untouched, same as before this feature");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC010");
    }

    #[test]
    fn insert_missing_sections_synthesizes_a_parameters_section_when_none_exists_at_all() {
        let source = format!(
            "{PARENT_WITH_ARG1_PY}\n\nclass Child(Parent):\n    \"\"\"Child class.\n\n    Does some child-specific things.\n    \"\"\"\n\n    def __init__(self, arg1):\n        pass\n"
        );
        let (output, diagnostics) = sync_inserting_missing_sections(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class Child").unwrap()..];
        let expected = "\
class Child(Parent):
    \"\"\"Child class.

    Does some child-specific things.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        assert_eq!(child_section, expected);
    }

    #[test]
    fn insert_missing_sections_inserts_ahead_of_an_existing_later_section_in_canonical_order() {
        let source = format!(
            "{PARENT_WITH_ARG1_PY}\n\nclass Child(Parent):\n    \"\"\"Child class.\n\n    Returns\n    -------\n    None\n        Nothing.\n    \"\"\"\n\n    def __init__(self, arg1):\n        pass\n"
        );
        let (output, diagnostics) = sync_inserting_missing_sections(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let child_section = &output[output.find("class Child").unwrap()..];
        let expected = "\
class Child(Parent):
    \"\"\"Child class.

    Parameters
    ----------
    arg1 : int
        Arg1 doc.

    Returns
    -------
    None
        Nothing.
    \"\"\"

    def __init__(self, arg1):
        pass
";
        assert_eq!(child_section, expected);
    }

    #[test]
    fn insert_missing_sections_never_creates_a_duplicate_header_over_a_malformed_existing_one() {
        // The Parameters header IS present here, just malformed (misindented arg line, so it
        // parses zero entries and fires DOC005) -- inserting a second header on top would either
        // duplicate it outright or splice against bookkeeping already known to be untrustworthy.
        let source = format!(
            "{PARENT_WITH_ARG1_PY}\n\nclass Child(Parent):\n    \"\"\"Child class.\n\n    Parameters\n    ----------\n     bad_indent_entry\n    \"\"\"\n\n    def __init__(self, arg1):\n        pass\n"
        );
        let (output, diagnostics) = sync_inserting_missing_sections(&source);
        assert_eq!(output, source, "a malformed existing header must never be touched, even with the option on");
        let codes: Vec<&str> = diagnostics.iter().map(|d| d.code).collect();
        assert!(codes.contains(&"DOC005"), "expected DOC005 for the malformed header, got: {diagnostics:?}");
        assert!(codes.contains(&"DOC010"), "expected DOC010 for the still-undocumented arg1, got: {diagnostics:?}");
    }

    #[test]
    fn insert_missing_sections_leaves_a_single_line_docstring_alone_and_still_diagnoses() {
        // No multi-line content anywhere to measure indentation from -- inserting at column 0
        // would produce a visibly misindented section, so this falls back to diagnosing DOC010
        // exactly like the option-off case, rather than guessing wrong.
        let source = format!("{PARENT_WITH_ARG1_PY}\n\nclass Child(Parent):\n    \"\"\"Child class.\"\"\"\n\n    def __init__(self, arg1):\n        pass\n");
        let (output, diagnostics) = sync_inserting_missing_sections(&source);
        assert_eq!(output, source);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC010");
    }

    #[test]
    fn insert_missing_sections_is_idempotent_on_rerun() {
        let source = format!(
            "{PARENT_WITH_ARG1_PY}\n\nclass Child(Parent):\n    \"\"\"Child class.\n\n    Does some child-specific things.\n    \"\"\"\n\n    def __init__(self, arg1):\n        pass\n"
        );
        let (first_pass, diagnostics) = sync_inserting_missing_sections(&source);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        let (second_pass, diagnostics) = sync_inserting_missing_sections(&first_pass);
        assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");
        assert_eq!(first_pass, second_pass, "a second run must not change anything further");
    }
}
