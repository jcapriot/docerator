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
    /// Whether this entry's own docstring was a raw (`r"""`) literal. Stamped in by the caller
    /// after parsing (parsing itself is style-agnostic and doesn't know the source docstring's
    /// prefix) — `false` at construction time inside a `DocStyle` impl. Used to guard copying an
    /// entry containing a backslash into a docstring with *different* raw-ness, where the same
    /// bytes would carry different escape semantics.
    pub is_raw: bool,
}

/// The result of parsing one docstring's parameter-documenting section(s), normalized to two
/// style-agnostic name -> entry maps — `primary` (numpydoc's `Parameters`) and `secondary`
/// (numpydoc's `Other Parameters`) — kept separate because the auto-sync engine needs to know
/// which section an entry belongs to (or should be inserted into): named signature parameters
/// are managed in `primary`, `expand=kwargs`-pulled parameters are managed in `secondary`.
/// Insertion order within each map is preserved (matches source order).
#[derive(Debug, Clone, Default)]
pub struct ParsedEntries {
    pub primary: IndexMap<String, ParamEntry>,
    pub secondary: IndexMap<String, ParamEntry>,
    /// Whether a `Parameters` section header was found at all, independent of whether any
    /// entries parsed under it (that's `primary.is_empty()`, a distinct case — a present-but-
    /// empty header is a malformed-indentation problem, diagnosed as `DOC005`, and must never be
    /// treated as "missing" or a second header would get synthesized right on top of it).
    pub has_primary_section: bool,
    /// Byte offset (relative to the parsed docstring text) of the first recognized section's own
    /// header line, of *any* kind — `Parameters`, `Returns`, `Notes`, whichever appears first in
    /// the text — or `None` if no section was recognized at all. Since `Parameters` is always the
    /// canonically-first section, this is exactly where a synthesized `Parameters` section needs
    /// to be inserted *before* to land in valid canonical order; `None` means there's nothing to
    /// insert before, so it belongs at the end of the docstring's own content instead.
    pub first_section_start: Option<usize>,
    /// The whitespace prefix shared by every section-header and arg-name line in this docstring
    /// (`compute_margin`'s result, rendered as a literal string) — the indentation a freshly
    /// synthesized section's own lines need, since there's no existing entry to borrow it from
    /// when the section doesn't exist yet at all.
    pub margin_indent: String,
}

impl ParsedEntries {
    /// Look up a name regardless of which section it's documented in.
    pub fn get(&self, name: &str) -> Option<&ParamEntry> {
        self.primary.get(name).or_else(|| self.secondary.get(name))
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Every entry from both sections, primary first, each in its own source order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ParamEntry)> {
        self.primary.iter().chain(self.secondary.iter())
    }

    pub fn is_empty(&self) -> bool {
        self.primary.is_empty() && self.secondary.is_empty()
    }

    /// Stamp every entry's `is_raw` with the raw-ness of the docstring they were just parsed
    /// out of — parsing itself is style-agnostic and has no notion of the source literal's `r`
    /// prefix, so the caller (which does know) fills this in right after `parse_entries` returns.
    pub fn set_is_raw(&mut self, is_raw: bool) {
        for entry in self.primary.values_mut().chain(self.secondary.values_mut()) {
            entry.is_raw = is_raw;
        }
    }
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

/// Every diagnostic code this tool can emit, paired with the severity it's constructed with at
/// its own call site — the single source of truth both `cache::static_code` (recovering a
/// `'static` code after a cache round-trip) and the CLI's rule-configuration validation
/// (`--rule`/`[tool.docerator.rules]`, checking a user-supplied code is real) key off of, so
/// adding a new `DOC0NN` diagnostic elsewhere only ever requires updating this one list to stay
/// consistent everywhere else that needs to enumerate "every known code".
pub const KNOWN_DIAGNOSTIC_CODES: &[(&str, Severity)] = &[
    ("DOC001", Severity::Warning),
    ("DOC002", Severity::Info),
    ("DOC003", Severity::Error),
    ("DOC004", Severity::Error),
    ("DOC005", Severity::Warning),
    ("DOC006", Severity::Warning),
    ("DOC007", Severity::Warning),
    ("DOC008", Severity::Warning),
    ("DOC009", Severity::Warning),
    ("DOC010", Severity::Warning),
    ("DOC011", Severity::Warning),
    ("DOC012", Severity::Error),
];

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
    /// indentation, using `newline` (`"\n"` or `"\r\n"`) for any line break the rendering itself
    /// introduces (e.g. between a `name : type` line and its description) — the caller owns
    /// deciding which convention matches the file actually being written to, since a
    /// style-agnostic renderer has no way to know that on its own.
    fn format_entry(&self, entry: &ParamEntry, indent: &str, newline: &str) -> String;

    /// Text needed to create the relevant section from scratch, for an entity that has no
    /// existing section to append an auto-managed entry into. Not yet implemented.
    fn synthesize_section(&self, section: ParamSectionKind) -> String;
}
