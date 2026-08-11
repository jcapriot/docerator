//! Reads `[tool.docerator]` from `pyproject.toml` for the project-wide style default — the
//! lowest-priority layer in the resolution order (entity directive > file directive > this >
//! hardcoded `numpydoc`; see `docerator_core::sync`).

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct PyProjectToml {
    tool: Option<ToolTable>,
}

#[derive(Debug, Default, Deserialize)]
struct ToolTable {
    docerator: Option<DoceratorConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DoceratorConfig {
    pub style: Option<String>,
    /// `[tool.docerator.rules]` — diagnostic code -> level (`"off"`, `"info"`, `"warning"`, or
    /// `"error"`), e.g. `DOC001 = "error"`. Kept as raw strings here and parsed/validated in
    /// `rules::merge` rather than as `RuleLevel` directly, so a typo in one entry only warns and
    /// drops that entry instead of failing this whole file's config load.
    #[serde(default)]
    pub rules: HashMap<String, String>,
    /// Mirrors `docerator_core::sync::SyncOptions::insert_missing_sections` — `--insert-missing-
    /// sections` on the CLI overrides this when both are given (the CLI flag wins, same
    /// precedent as every other layered setting here).
    pub insert_missing_sections: Option<bool>,
}

/// Look for `pyproject.toml` directly inside `project_root` and read its `[tool.docerator]`
/// table, if any. A missing file, a missing table, or a file that fails to parse all resolve to
/// "no project config" rather than an error — this is a convenience default, not something
/// required to run the tool.
pub fn load(project_root: &Path) -> DoceratorConfig {
    let path = project_root.join("pyproject.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return DoceratorConfig::default();
    };
    let Ok(parsed) = toml::from_str::<PyProjectToml>(&text) else {
        eprintln!("warning: {}: failed to parse, ignoring [tool.docerator] config", path.display());
        return DoceratorConfig::default();
    };
    parsed.tool.and_then(|t| t.docerator).unwrap_or_default()
}
