use ruff_python_ast::{Expr, ModModule, Stmt};
use ruff_text_size::{Ranged, TextRange};

/// A class or function whose body starts with a bare string-literal expression (its docstring),
/// located but not yet parsed/interpreted — this module only finds *where* docstrings are, the
/// numpydoc-specific parsing lives behind the `DocStyle` trait (not yet implemented).
#[derive(Debug, Clone)]
pub struct DocstringEntity {
    pub name: String,
    /// Byte range of the docstring string-literal node, quotes included.
    pub literal_range: TextRange,
}

/// Walk a module and collect every class/function definition that has a docstring, at any
/// nesting depth (methods inside classes, nested functions, etc.).
pub fn find_docstrings(module: &ModModule) -> Vec<DocstringEntity> {
    let mut out = Vec::new();
    visit_body(&module.body, &mut out);
    out
}

fn visit_body(body: &[Stmt], out: &mut Vec<DocstringEntity>) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class_def) => {
                if let Some(range) = docstring_literal_range(&class_def.body) {
                    out.push(DocstringEntity {
                        name: class_def.name.to_string(),
                        literal_range: range,
                    });
                }
                visit_body(&class_def.body, out);
            }
            Stmt::FunctionDef(func_def) => {
                if let Some(range) = docstring_literal_range(&func_def.body) {
                    out.push(DocstringEntity {
                        name: func_def.name.to_string(),
                        literal_range: range,
                    });
                }
                visit_body(&func_def.body, out);
            }
            _ => {}
        }
    }
}

/// If `body`'s first statement is a bare string-literal expression, return that literal's range.
fn docstring_literal_range(body: &[Stmt]) -> Option<TextRange> {
    let Stmt::Expr(expr_stmt) = body.first()? else {
        return None;
    };
    let Expr::StringLiteral(string_lit) = expr_stmt.value.as_ref() else {
        return None;
    };
    Some(string_lit.range())
}
