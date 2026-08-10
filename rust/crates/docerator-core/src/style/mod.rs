pub mod numpydoc;

use indexmap::IndexMap;
use ruff_text_size::TextRange;

/// One parsed parameter documentation entry: a name, its optional type description, its
/// optional long-form description, and the exact byte range of its whole text block (the
/// `name [: type]` line through its trailing description lines) within the text that was
/// parsed — used later to splice/insert without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamEntry {
    pub name: String,
    pub type_description: Option<String>,
    pub description: Option<String>,
    pub range: TextRange,
}

/// The result of parsing one docstring's parameter-documenting section(s), normalized to a
/// style-agnostic name -> entry map. Insertion order is preserved (matches source order),
/// mirroring the ordering guarantees Python dicts already gave the original implementation.
#[derive(Debug, Clone, Default)]
pub struct ParsedEntries {
    pub entries: IndexMap<String, ParamEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    pub range: TextRange,
}

/// Which of the two parameter-doc roles the auto-sync engine cares about, independent of how
/// any particular style spells them (numpydoc: `Parameters` / `Other Parameters`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamSectionKind {
    Primary,
    Secondary,
}

/// A docstring dialect. The auto-sync engine (not yet implemented) is written entirely in
/// terms of this trait's output — it never touches numpydoc-specific syntax directly, so a
/// future Google/Sphinx style is a new impl + registry entry, not a change to the engine.
pub trait DocStyle {
    /// Locate this style's parameter-documenting section(s) and parse them into normalized
    /// entries with byte ranges relative to `docstring_text`.
    fn parse_entries(&self, docstring_text: &str) -> (ParsedEntries, Vec<Diagnostic>);

    /// Render one entry back into this style's on-disk text, at the caller-supplied ambient
    /// indentation. Not yet implemented — needed starting M2, when entries are spliced back
    /// into real files.
    fn format_entry(&self, entry: &ParamEntry, indent: &str) -> String;

    /// Text needed to create the relevant section from scratch, for an entity that has no
    /// existing section to append an auto-managed entry into. Not yet implemented.
    fn synthesize_section(&self, section: ParamSectionKind) -> String;
}
