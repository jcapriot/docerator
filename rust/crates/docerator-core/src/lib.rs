pub mod cache;
pub mod directives;
pub mod docstring;
pub mod edit;
pub mod location;
pub mod model;
pub mod parse;
pub mod project;
pub mod provenance;
pub mod source;
pub mod style;
pub mod sync;

pub use edit::TextEdit;
pub use source::SourceFile;

/// Parse `text`, locate its docstrings, and apply whatever edits the (currently empty) rewrite
/// pipeline produces. M0 scope: no auto-sync logic exists yet, so this always returns `text`
/// unchanged — its purpose is to prove the parse -> locate -> splice -> write-back pipeline is
/// byte-identical end to end (round-trip fidelity) before any real rewriting is layered on top.
pub fn process_source(text: &str) -> Result<String, ruff_python_parser::ParseError> {
    let parsed = parse::parse(text)?;
    let _entities = docstring::find_docstrings(parsed.syntax());

    let edits: Vec<TextEdit> = Vec::new();
    Ok(edit::apply_edits(text, edits))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "class Parent:\n    \"\"\"A docstring.\n\n    Parameters\n    ----------\n    arg1 : int\n        First arg.\n    \"\"\"\n\n    def __init__(self, arg1):\n        pass\n\n    def a_method(self, x):\n        \"\"\"A method.\n\n        Parameters\n        ----------\n        x : float\n            The value.\n        \"\"\"\n";

    #[test]
    fn round_trip_is_byte_identical_with_no_rewrite_logic() {
        let output = process_source(FIXTURE).expect("fixture must parse");
        assert_eq!(output, FIXTURE);
    }

    #[test]
    fn round_trip_preserves_crlf_line_endings() {
        let crlf_fixture = FIXTURE.replace('\n', "\r\n");
        let output = process_source(&crlf_fixture).expect("fixture must parse");
        assert_eq!(output, crlf_fixture);
        assert!(output.contains("\r\n"));
        assert!(!output.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn finds_docstrings_on_class_and_nested_method() {
        let parsed = parse::parse(FIXTURE).expect("fixture must parse");
        let entities = docstring::find_docstrings(parsed.syntax());
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Parent", "a_method"]);
    }
}
