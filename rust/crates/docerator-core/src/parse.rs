use ruff_python_ast::ModModule;
use ruff_python_parser::{parse_module, ParseError, Parsed};

/// Parse a Python source string into its module AST.
///
/// A thin wrapper kept as its own module so the rest of `docerator-core` never imports
/// `ruff_python_parser` directly — if the parser crate's API shifts, only this file changes.
pub fn parse(source: &str) -> Result<Parsed<ModModule>, ParseError> {
    parse_module(source)
}
