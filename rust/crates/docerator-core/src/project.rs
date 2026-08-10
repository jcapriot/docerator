//! Assembles per-file models (`model.rs`) into a whole-project view and resolves base-class
//! references across files: relative imports (`from . import X`, `from ..pkg import X`),
//! absolute imports (including src-layout, where the package's own name has to be *discovered*
//! rather than assumed), and re-exports (a name imported into a package's `__init__.py` from
//! one of its submodules, then imported again from the package itself elsewhere).
//!
//! Module-name discovery uses one heuristic, applied uniformly (no hardcoded "src" special
//! case): from a file's own directory, walk upward through ancestor directories for as long as
//! each one contains an `__init__.py` (making it a package); the first ancestor that doesn't is
//! the file's source root. This naturally derives "mypackage" as the package name for
//! `src/mypackage/__init__.py` (since `src/` itself has no `__init__.py`) without ever needing
//! to know the string "src" — the same rule handles a flat layout, a `lib/` layout, or anything
//! else, uniformly.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::model::{self, BaseRef, FileModel, ImportSource, ImportedSymbol};
use crate::parse;
use crate::style::{Diagnostic, Severity};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClassId {
    pub module: String,
    pub name: String,
}

pub struct ProjectFile {
    pub path: PathBuf,
    pub module_name: String,
    pub is_package_init: bool,
    pub model: FileModel,
}

pub struct ProjectModel {
    pub files: Vec<ProjectFile>,
    module_index: HashMap<String, usize>,
}

impl ProjectModel {
    pub fn file_for_module(&self, module: &str) -> Option<&ProjectFile> {
        self.module_index.get(module).map(|&i| &self.files[i])
    }

    pub fn class(&self, id: &ClassId) -> Option<(&ProjectFile, &model::ClassModel)> {
        let file = self.file_for_module(&id.module)?;
        let class = file.model.classes.get(&id.name)?;
        Some((file, class))
    }
}

/// Parse and model every file, skipping (and diagnosing) any that fail to parse rather than
/// aborting the whole project — one syntactically-broken file shouldn't block every other file
/// in the project from being processed. Diagnostics are grouped by the file they belong to,
/// since a project spans more than one.
pub fn build_project(files: &[(PathBuf, String)], diagnostics_by_file: &mut HashMap<PathBuf, Vec<Diagnostic>>) -> ProjectModel {
    let package_dirs: HashSet<PathBuf> = files
        .iter()
        .filter(|(path, _)| is_init_file(path))
        .filter_map(|(path, _)| path.parent().map(Path::to_path_buf))
        .collect();

    let mut project_files = Vec::with_capacity(files.len());
    let mut module_index = HashMap::new();

    for (path, text) in files {
        let file_diagnostics = diagnostics_by_file.entry(path.clone()).or_default();
        let parsed = match parse::parse(text) {
            Ok(parsed) => parsed,
            Err(err) => {
                file_diagnostics.push(Diagnostic {
                    code: "DOC012",
                    severity: Severity::Error,
                    message: format!("failed to parse, skipped ({err})"),
                    range: ruff_text_size::TextRange::default(),
                });
                continue;
            }
        };
        let file_model = model::build_file_model(text, parsed.syntax(), file_diagnostics);
        let is_package_init = is_init_file(path);
        let module_name = compute_module_name(path, is_package_init, &package_dirs);

        module_index.insert(module_name.clone(), project_files.len());
        project_files.push(ProjectFile {
            path: path.clone(),
            module_name,
            is_package_init,
            model: file_model,
        });
    }

    ProjectModel { files: project_files, module_index }
}

fn is_init_file(path: &Path) -> bool {
    path.file_name().and_then(|f| f.to_str()) == Some("__init__.py")
}

fn compute_module_name(path: &Path, is_package_init: bool, package_dirs: &HashSet<PathBuf>) -> String {
    let mut segments: Vec<String> = Vec::new();
    if !is_package_init {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            segments.push(stem.to_string());
        }
    }
    let mut dir = path.parent();
    while let Some(d) = dir {
        if !package_dirs.contains(d) {
            break;
        }
        if let Some(name) = d.file_name().and_then(|n| n.to_str()) {
            segments.push(name.to_string());
        }
        dir = d.parent();
    }
    segments.reverse();
    segments.join(".")
}

/// Resolve a `from`-import's module reference (absolute or relative) to a dotted module name,
/// relative to the importing file's own package identity.
fn resolve_import_source(file: &ProjectFile, source: &ImportSource) -> Option<String> {
    match source {
        ImportSource::Absolute(m) => Some(m.clone()),
        ImportSource::Relative { level, submodule } => {
            resolve_relative(&file.module_name, file.is_package_init, *level, submodule.as_deref())
        }
    }
}

fn resolve_relative(current_module: &str, is_package_init: bool, level: u32, submodule: Option<&str>) -> Option<String> {
    if level == 0 {
        return None;
    }
    let mut own_package: Vec<&str> = current_module.split('.').filter(|s| !s.is_empty()).collect();
    if !is_package_init {
        // A regular module's *containing* package is everything but its own last segment; an
        // `__init__.py`'s containing package (for `level == 1`) is itself.
        own_package.pop();
    }
    let pops = (level - 1) as usize;
    if pops > own_package.len() {
        return None; // walked above the project root
    }
    let base = &own_package[..own_package.len() - pops];
    let mut segments: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    if let Some(sub) = submodule {
        segments.extend(sub.split('.').filter(|s| !s.is_empty()).map(str::to_string));
    }
    Some(segments.join("."))
}

/// Resolve `name` inside `module`, following re-exports (a name imported into that module from
/// somewhere else, rather than defined there) transitively — this is what makes `from
/// mypackage import Foo` work when `Foo` is really defined in `mypackage/_internal.py` and only
/// re-exported via `mypackage/__init__.py`'s own `from ._internal import Foo`.
pub fn resolve_symbol(project: &ProjectModel, module: &str, name: &str) -> Option<ClassId> {
    let mut visited = HashSet::new();
    resolve_symbol_inner(project, module, name, &mut visited)
}

fn resolve_symbol_inner(project: &ProjectModel, module: &str, name: &str, visited: &mut HashSet<(String, String)>) -> Option<ClassId> {
    if !visited.insert((module.to_string(), name.to_string())) {
        return None; // circular re-export chain -- give up rather than loop forever
    }
    let file = project.file_for_module(module)?;
    if file.model.classes.contains_key(name) {
        return Some(ClassId {
            module: module.to_string(),
            name: name.to_string(),
        });
    }
    match file.model.imports.get(name)? {
        ImportedSymbol::Name { module: source, name: target_name } => {
            let target_module = resolve_import_source(file, source)?;
            resolve_symbol_inner(project, &target_module, target_name, visited)
        }
        ImportedSymbol::Module(_) => None,
    }
}

/// Resolve a class's base-class expression, as recorded by `model.rs`, to the file+class that
/// actually defines it — `None` means either a dynamic expression that was never resolvable
/// (not diagnosed, it was never going to be a statically-known class) or a name/import that
/// looked resolvable but couldn't be found in the project (diagnosed by the caller as DOC002,
/// since it *did* look like an attempt at referencing a real class).
pub fn resolve_base_ref(project: &ProjectModel, file: &ProjectFile, base_ref: &BaseRef) -> Option<ClassId> {
    match base_ref {
        BaseRef::Name(name) => match file.model.imports.get(name) {
            Some(ImportedSymbol::Name { module, name: target_name }) => {
                let target_module = resolve_import_source(file, module)?;
                resolve_symbol(project, &target_module, target_name)
            }
            Some(ImportedSymbol::Module(_)) => None,
            None => {
                if file.model.classes.contains_key(name) {
                    Some(ClassId {
                        module: file.module_name.clone(),
                        name: name.clone(),
                    })
                } else {
                    None
                }
            }
        },
        BaseRef::Attribute(segments) => {
            if segments.len() < 2 {
                return None;
            }
            let (head, rest) = segments.split_first()?;
            let (class_name, module_segments) = rest.split_last()?;
            match file.model.imports.get(head) {
                Some(ImportedSymbol::Module(dotted)) => {
                    let mut target_module = dotted.clone();
                    for seg in module_segments {
                        target_module.push('.');
                        target_module.push_str(seg);
                    }
                    resolve_symbol(project, &target_module, class_name)
                }
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module_name_for(path: &str, all_paths: &[&str]) -> String {
        let files: Vec<(PathBuf, String)> = all_paths.iter().map(|p| (PathBuf::from(p), String::new())).collect();
        let package_dirs: HashSet<PathBuf> = files
            .iter()
            .filter(|(p, _)| is_init_file(p))
            .filter_map(|(p, _)| p.parent().map(Path::to_path_buf))
            .collect();
        let target = PathBuf::from(path);
        compute_module_name(&target, is_init_file(&target), &package_dirs)
    }

    #[test]
    fn flat_layout_module_name() {
        let all = ["pkg/__init__.py", "pkg/sub/__init__.py", "pkg/sub/mod.py"];
        assert_eq!(module_name_for("pkg/sub/mod.py", &all), "pkg.sub.mod");
        assert_eq!(module_name_for("pkg/sub/__init__.py", &all), "pkg.sub");
        assert_eq!(module_name_for("pkg/__init__.py", &all), "pkg");
    }

    #[test]
    fn src_layout_module_name_has_no_leading_src_segment() {
        let all = [
            "src/mypackage/__init__.py",
            "src/mypackage/sub/__init__.py",
            "src/mypackage/sub/mod.py",
        ];
        assert_eq!(module_name_for("src/mypackage/__init__.py", &all), "mypackage");
        assert_eq!(module_name_for("src/mypackage/sub/mod.py", &all), "mypackage.sub.mod");
    }

    #[test]
    fn standalone_module_with_no_package_at_all() {
        let all = ["standalone.py"];
        assert_eq!(module_name_for("standalone.py", &all), "standalone");
    }

    #[test]
    fn relative_import_resolution() {
        // from a regular module
        assert_eq!(resolve_relative("pkg.sub.mod", false, 1, None).as_deref(), Some("pkg.sub"));
        assert_eq!(resolve_relative("pkg.sub.mod", false, 1, Some("sibling")).as_deref(), Some("pkg.sub.sibling"));
        assert_eq!(resolve_relative("pkg.sub.mod", false, 2, None).as_deref(), Some("pkg"));
        assert_eq!(resolve_relative("pkg.sub.mod", false, 2, Some("other")).as_deref(), Some("pkg.other"));
        // from an __init__.py itself
        assert_eq!(resolve_relative("pkg.sub", true, 1, None).as_deref(), Some("pkg.sub"));
        assert_eq!(resolve_relative("pkg.sub", true, 1, Some("mod")).as_deref(), Some("pkg.sub.mod"));
        assert_eq!(resolve_relative("pkg.sub", true, 2, None).as_deref(), Some("pkg"));
        // walking above the project root
        assert_eq!(resolve_relative("pkg", false, 3, None), None);
    }
}
