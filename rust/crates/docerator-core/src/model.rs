//! Extracts a per-class, per-method model (signature + docstring location) from a single parsed
//! file — the input `project.rs` assembles into a whole-project view and `sync.rs` consumes.
//! This module only ever looks at ONE file's own AST; it records base-class expressions and
//! import statements in a form `project.rs` can resolve against the *whole* project afterward —
//! it never itself decides whether a name is same-file, cross-file, or unresolvable.

use indexmap::IndexMap;
use ruff_python_ast::{Expr, ModModule, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::directives::{self, Directives};
use crate::style::Diagnostic;

/// A base-class expression, exactly as written, in a form cheap to resolve later without
/// re-inspecting the AST: `class Foo(Bar):` -> `Name("Bar")`; `class Foo(pkg.sub.Bar):` ->
/// `Attribute(["pkg", "sub", "Bar"])`. Anything else (a call expression, a subscript, ...) is
/// dynamic and never resolvable — `project.rs` treats it as opaque, no diagnostic (it was never
/// going to be a statically-known class).
#[derive(Debug, Clone)]
pub enum BaseRef {
    Name(String),
    /// Dotted attribute chain, e.g. `pkg.sub.Bar` -> `["pkg", "sub", "Bar"]`. The last segment
    /// is always the class name; everything before it is a module path to resolve through the
    /// leading segment's import binding.
    Attribute(Vec<String>),
}

/// One `import`/`from ... import ...` statement's contribution to this file's local namespace.
#[derive(Debug, Clone)]
pub enum ImportedSymbol {
    /// `import a.b.c` (binds `a`) or `import a.b.c as x` (binds `x`) — the local name refers to
    /// a whole module; a later `name.attr` attribute access resolves `attr` within it.
    Module(String),
    /// `from a.b import c` (binds `c`) or `from a.b import c as d` (binds `d`) — the local name
    /// refers to one specific symbol inside that module, which may itself be a re-export
    /// (`project.rs` follows the chain).
    Name { module: ImportSource, name: String },
}

/// Where a `from ... import ...`'s module comes from — resolving relative imports needs the
/// importing file's own package identity, which only `project.rs` (whole-project view) has, so
/// this stores the raw ingredients rather than a resolved dotted name.
#[derive(Debug, Clone)]
pub enum ImportSource {
    /// `from a.b import c` — an absolute dotted module path.
    Absolute(String),
    /// `from . import c` / `from .sib import c` / `from ..pkg import c` — `level` dots (1 = the
    /// current package) and an optional trailing dotted submodule path (`None` for `from . import c`).
    Relative { level: u32, submodule: Option<String> },
}

#[derive(Debug, Clone)]
pub struct DocstringInfo {
    /// Absolute (file-coordinate) byte range of the docstring's interior text, quotes excluded.
    pub inner_range: TextRange,
    /// The interior text, verbatim (escapes not decoded).
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct ParamSignature {
    /// Named parameters (positional-only, positional-or-keyword, keyword-only), in signature
    /// order, with a leading `self`/`cls` dropped.
    pub names: Vec<String>,
    pub has_var_keyword: bool,
}

#[derive(Debug, Clone)]
pub struct MethodModel {
    pub docstring: Option<DocstringInfo>,
    pub signature: ParamSignature,
    /// Directives from a `# docerator:` comment directly above this method (through its own
    /// decorators, if any) — does NOT include the enclosing class's directives; merging class-
    /// and method-level directives is `sync.rs`'s job, since only it knows which directives are
    /// class-broadcast (`skip`) versus `__init__`-only (`override`, `style`).
    pub directives: Directives,
}

#[derive(Debug, Clone)]
pub struct ClassModel {
    pub base_refs: Vec<BaseRef>,
    pub methods: IndexMap<String, MethodModel>,
    /// Directives from a `# docerator:` comment directly above the class itself.
    pub directives: Directives,
    /// The `class` statement's own range — used to anchor diagnostics about its base classes.
    pub range: TextRange,
}

#[derive(Debug, Clone, Default)]
pub struct FileModel {
    pub classes: IndexMap<String, ClassModel>,
    /// The project-wide-style-equivalent default declared at file scope: the last
    /// `# docerator: style=...` comment appearing before the first top-level class/function.
    pub file_level_style: Option<String>,
    /// Local-name -> imported-symbol bindings from every top-level `import`/`from ... import`
    /// statement (not ones nested inside a function/class/if/try body — base classes are
    /// realistically always resolved against plain module-level imports).
    pub imports: IndexMap<String, ImportedSymbol>,
}

pub fn build_file_model(source: &str, module: &ModModule, diagnostics: &mut Vec<Diagnostic>) -> FileModel {
    let mut classes = IndexMap::new();
    collect_classes(source, &module.body, &mut classes, diagnostics);

    let first_top_level_def_start = module.body.iter().find_map(|stmt| match stmt {
        Stmt::ClassDef(c) => Some(c.range().start()),
        Stmt::FunctionDef(f) => Some(f.range().start()),
        _ => None,
    });
    let file_level_style =
        first_top_level_def_start.and_then(|offset| directives::file_level_style(source, offset));

    let imports = collect_imports(&module.body);

    FileModel { classes, file_level_style, imports }
}

fn collect_imports(body: &[Stmt]) -> IndexMap<String, ImportedSymbol> {
    let mut imports = IndexMap::new();
    for stmt in body {
        match stmt {
            Stmt::Import(import) => {
                for alias in &import.names {
                    let dotted = alias.name.to_string();
                    match &alias.asname {
                        // `import a.b.c as x` binds `x` directly to the full target module —
                        // `x` IS `a.b.c`, nothing to walk.
                        Some(asname) => {
                            imports.insert(asname.to_string(), ImportedSymbol::Module(dotted));
                        }
                        // `import a.b.c` (no `as`) binds only the top-level name `a`, to module
                        // `a` itself — `a.b`/`a.b.c` are only reachable afterward via attribute
                        // access through `a` (Python sets submodules as attributes of their
                        // parent package on import), which `project.rs`'s attribute-chain
                        // resolution walks starting from THIS binding, so it must point at just
                        // `a`, not the full dotted path.
                        None => {
                            let top_level = dotted.split('.').next().unwrap_or(&dotted).to_string();
                            imports.insert(top_level.clone(), ImportedSymbol::Module(top_level));
                        }
                    }
                }
            }
            Stmt::ImportFrom(import_from) => {
                let module_path = import_from.module.as_ref().map(|m| m.to_string());
                let source = if import_from.level > 0 {
                    ImportSource::Relative {
                        level: import_from.level,
                        submodule: module_path,
                    }
                } else {
                    ImportSource::Absolute(module_path.unwrap_or_default())
                };
                for alias in &import_from.names {
                    let imported_name = alias.name.to_string();
                    if imported_name == "*" {
                        // Star imports are dynamic/unbounded -- never resolvable, and don't
                        // bind any specific local name we could look up later.
                        continue;
                    }
                    let local_name = alias.asname.as_ref().map(|a| a.to_string()).unwrap_or_else(|| imported_name.clone());
                    imports.insert(
                        local_name,
                        ImportedSymbol::Name {
                            module: source.clone(),
                            name: imported_name,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    imports
}

fn collect_classes(source: &str, body: &[Stmt], out: &mut IndexMap<String, ClassModel>, diagnostics: &mut Vec<Diagnostic>) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class_def) => {
                out.insert(class_def.name.to_string(), build_class_model(source, class_def, diagnostics));
                collect_classes(source, &class_def.body, out, diagnostics);
            }
            Stmt::FunctionDef(func_def) => {
                collect_classes(source, &func_def.body, out, diagnostics);
            }
            _ => {}
        }
    }
}

fn build_class_model(source: &str, class_def: &StmtClassDef, diagnostics: &mut Vec<Diagnostic>) -> ClassModel {
    let base_refs = class_def
        .arguments
        .as_ref()
        .map(|args| args.args.iter().filter_map(base_ref).collect())
        .unwrap_or_default();

    let class_docstring = leading_docstring(source, &class_def.body);
    let class_anchor = class_def
        .decorator_list
        .first()
        .map(|d| d.range().start())
        .unwrap_or_else(|| class_def.range().start());
    let class_directives = directives::resolve_directives_for(source, class_anchor, diagnostics);

    let mut methods = IndexMap::new();
    for stmt in &class_def.body {
        if let Stmt::FunctionDef(func_def) = stmt {
            let own_doc = leading_docstring(source, &func_def.body);
            let docstring = if func_def.name.as_str() == "__init__" && own_doc.is_none() {
                class_docstring.clone()
            } else {
                own_doc
            };
            let signature = extract_signature(func_def);
            let method_anchor = func_def
                .decorator_list
                .first()
                .map(|d| d.range().start())
                .unwrap_or_else(|| func_def.range().start());
            let method_directives = directives::resolve_directives_for(source, method_anchor, diagnostics);
            methods.insert(
                func_def.name.to_string(),
                MethodModel {
                    docstring,
                    signature,
                    directives: method_directives,
                },
            );
        }
    }

    ClassModel {
        base_refs,
        methods,
        directives: class_directives,
        range: class_def.range(),
    }
}

/// `Bar` -> `Name("Bar")`; `pkg.sub.Bar` -> `Attribute(["pkg", "sub", "Bar"])`; anything else
/// (a call, subscript, etc.) is a dynamic base expression and never resolvable, so `None`.
fn base_ref(expr: &Expr) -> Option<BaseRef> {
    match expr {
        Expr::Name(name) => Some(BaseRef::Name(name.id.to_string())),
        Expr::Attribute(_) => flatten_attribute_chain(expr).map(BaseRef::Attribute),
        _ => None,
    }
}

/// Walks a (possibly nested) `Expr::Attribute` chain rooted in an `Expr::Name` into its
/// dotted-segment form, e.g. `pkg.sub.Bar` -> `["pkg", "sub", "Bar"]`. Returns `None` if the
/// chain doesn't bottom out in a plain name (e.g. `foo().Bar`).
fn flatten_attribute_chain(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Name(name) => Some(vec![name.id.to_string()]),
        Expr::Attribute(attr) => {
            let mut segments = flatten_attribute_chain(&attr.value)?;
            segments.push(attr.attr.to_string());
            Some(segments)
        }
        _ => None,
    }
}

fn extract_signature(func_def: &StmtFunctionDef) -> ParamSignature {
    let params = &func_def.parameters;
    let mut names: Vec<String> = Vec::new();
    for p in params.posonlyargs.iter() {
        names.push(p.parameter.name.to_string());
    }
    for p in params.args.iter() {
        names.push(p.parameter.name.to_string());
    }
    for p in params.kwonlyargs.iter() {
        names.push(p.parameter.name.to_string());
    }
    if matches!(names.first().map(String::as_str), Some("self") | Some("cls")) {
        names.remove(0);
    }
    let has_var_keyword = params.kwarg.is_some();
    ParamSignature { names, has_var_keyword }
}

fn leading_docstring(source: &str, body: &[Stmt]) -> Option<DocstringInfo> {
    let Stmt::Expr(expr_stmt) = body.first()? else {
        return None;
    };
    let Expr::StringLiteral(string_lit) = expr_stmt.value.as_ref() else {
        return None;
    };
    let (inner_range, text) = docstring_inner(source, string_lit.range())?;
    Some(DocstringInfo {
        inner_range,
        text: text.to_string(),
    })
}

/// Given a string-literal node's full range (quotes included), find the byte range and text of
/// its interior, handling `"""`/`'''`/`"`/`'` with an optional alphabetic prefix (`r`, `f`, `u`,
/// `b`, and combinations). Returns `None` for shapes we don't recognize (should not happen for
/// a literal `ruff_python_ast` itself already classified as a string literal).
fn docstring_inner(source: &str, literal_range: TextRange) -> Option<(TextRange, &str)> {
    let full = &source[usize::from(literal_range.start())..usize::from(literal_range.end())];
    let prefix_len = full.bytes().take_while(u8::is_ascii_alphabetic).count();
    let rest = &full[prefix_len..];
    let quote_len = if rest.starts_with("\"\"\"") || rest.starts_with("'''") {
        3
    } else if rest.starts_with('"') || rest.starts_with('\'') {
        1
    } else {
        return None;
    };
    let inner_start = prefix_len + quote_len;
    let inner_end = full.len().checked_sub(quote_len)?;
    if inner_end < inner_start {
        return None;
    }
    let inner_text = &full[inner_start..inner_end];
    let abs_start = literal_range.start() + TextSize::try_from(inner_start).unwrap();
    let abs_end = literal_range.start() + TextSize::try_from(inner_end).unwrap();
    Some((TextRange::new(abs_start, abs_end), inner_text))
}
