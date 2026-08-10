//! Extracts a per-class, per-method model (signature + docstring location) from a parsed file —
//! the input the auto-sync engine (`sync.rs`) needs. M2 scope: same-file base-class resolution
//! only (a base class expression that isn't a plain `Name` in this file is simply unresolvable
//! and treated as no ancestor); cross-file resolution is deferred to a later milestone.

use indexmap::IndexMap;
use ruff_python_ast::{Expr, ModModule, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

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
}

#[derive(Debug, Clone, Default)]
pub struct ClassModel {
    pub base_names: Vec<String>,
    pub methods: IndexMap<String, MethodModel>,
}

#[derive(Debug, Clone, Default)]
pub struct FileModel {
    pub classes: IndexMap<String, ClassModel>,
}

pub fn build_file_model(source: &str, module: &ModModule) -> FileModel {
    let mut classes = IndexMap::new();
    collect_classes(source, &module.body, &mut classes);
    FileModel { classes }
}

fn collect_classes(source: &str, body: &[Stmt], out: &mut IndexMap<String, ClassModel>) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class_def) => {
                out.insert(class_def.name.to_string(), build_class_model(source, class_def));
                collect_classes(source, &class_def.body, out);
            }
            Stmt::FunctionDef(func_def) => {
                collect_classes(source, &func_def.body, out);
            }
            _ => {}
        }
    }
}

fn build_class_model(source: &str, class_def: &StmtClassDef) -> ClassModel {
    let base_names = class_def
        .arguments
        .as_ref()
        .map(|args| args.args.iter().filter_map(simple_name).collect())
        .unwrap_or_default();

    let class_docstring = leading_docstring(source, &class_def.body);

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
            methods.insert(func_def.name.to_string(), MethodModel { docstring, signature });
        }
    }

    ClassModel { base_names, methods }
}

fn simple_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(name) => Some(name.id.to_string()),
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
