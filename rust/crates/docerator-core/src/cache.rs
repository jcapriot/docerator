//! On-disk cache for skipping redundant work across separate CLI invocations — the "fast
//! follow" the original plan deferred (in-memory memoization within one run was v1's scope).
//!
//! Scope, deliberately: this skips re-running the numpydoc-parsing + MRO-merge + diagnostic-
//! generation + edit-computation work (`sync.rs`'s per-method loop) for a file whose content
//! and whose *entire transitive ancestor chain's* content are both unchanged since the cached
//! run — that work (string parsing, entry comparison, text formatting) is the actual
//! distinctive cost this tool adds on top of parsing. It does NOT skip re-parsing the file's
//! AST (`project::build_project` always reparses every file) — building a "shadow" file model
//! for skipped files well enough to keep cross-file import/re-export resolution correct for
//! *other* files that might reference them turned out to add real correctness risk for a
//! secondary win, since ruff's own parser is already fast. Revisit only if profiling shows
//! parsing itself dominates.
//!
//! Validity is checked at two levels:
//! - **Global key** (tool version, project-wide style default, the full set of module names
//!   present): any change invalidates the whole cache. This is the conservative, safe answer to
//!   "a new file could resolve an import that used to be opaque" — since an unresolvable import
//!   was never recorded as a dependency (there was nothing to record), a per-file fingerprint
//!   alone can't detect that a *new* file might change the answer.
//! - **Per-file dependency fingerprint**: for a file whose own content hash still matches, its
//!   recorded `(module, content_hash)` pairs for every file transitively reachable through its
//!   classes' base-class chains (including itself) must all still match current hashes.

use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use ruff_text_size::{TextRange, TextSize};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::style::{Diagnostic, ParamEntry, Severity};

pub const CACHE_FORMAT_VERSION: u32 = 1;
const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

pub type MethodViews = IndexMap<String, IndexMap<String, ParamEntry>>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Cache {
    pub format_version: u32,
    pub tool_version: String,
    pub global_key: String,
    pub files: HashMap<PathBuf, CachedFile>,
}

impl Cache {
    pub fn is_stale(&self) -> bool {
        self.format_version != CACHE_FORMAT_VERSION || self.tool_version != TOOL_VERSION
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFile {
    pub content_hash: String,
    /// `(module_name, content_hash)` for every file transitively reachable through this file's
    /// classes' base-class chains, including this file's own entry.
    pub dependencies: Vec<(String, String)>,
    pub output_text: String,
    pub diagnostics: Vec<CachedDiagnostic>,
    /// Per class name in this file, its resolved authored+inherited view.
    pub classes: HashMap<String, CachedMethodViews>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedDiagnostic {
    pub code: String,
    pub severity: CachedSeverity,
    pub message: String,
    pub range_start: u32,
    pub range_end: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CachedSeverity {
    Error,
    Warning,
    Info,
}

pub type CachedMethodViews = IndexMap<String, IndexMap<String, CachedParamEntry>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedParamEntry {
    pub name: String,
    pub type_description: Option<String>,
    pub description: Option<String>,
    pub range_start: u32,
    pub range_end: u32,
}

pub fn hash_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn compute_global_key(project_default_style: Option<&str>, module_names: &[String]) -> String {
    let mut sorted: Vec<&str> = module_names.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(project_default_style.unwrap_or("").as_bytes());
    hasher.update([0u8]);
    for name in sorted {
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

pub fn dependencies_still_valid(dependencies: &[(String, String)], module_hashes: &HashMap<String, String>) -> bool {
    dependencies.iter().all(|(module, hash)| module_hashes.get(module) == Some(hash))
}

fn severity_to_cached(severity: Severity) -> CachedSeverity {
    match severity {
        Severity::Error => CachedSeverity::Error,
        Severity::Warning => CachedSeverity::Warning,
        Severity::Info => CachedSeverity::Info,
    }
}

fn cached_to_severity(severity: CachedSeverity) -> Severity {
    match severity {
        CachedSeverity::Error => Severity::Error,
        CachedSeverity::Warning => Severity::Warning,
        CachedSeverity::Info => Severity::Info,
    }
}

/// `Diagnostic::code` is `&'static str` (every real diagnostic constructs it from a literal);
/// recovering a `'static` reference after deserializing an owned `String` means mapping back
/// through the known set rather than leaking memory — this is the one place that list needs to
/// be kept in sync with the codes actually used elsewhere (a mismatch just means the diagnostic
/// prints as `DOC000` instead of its real code, never a crash).
fn static_code(code: &str) -> &'static str {
    match code {
        "DOC001" => "DOC001",
        "DOC002" => "DOC002",
        "DOC003" => "DOC003",
        "DOC004" => "DOC004",
        "DOC005" => "DOC005",
        "DOC006" => "DOC006",
        "DOC007" => "DOC007",
        "DOC008" => "DOC008",
        "DOC009" => "DOC009",
        "DOC010" => "DOC010",
        "DOC011" => "DOC011",
        "DOC012" => "DOC012",
        _ => "DOC000",
    }
}

pub fn diagnostic_to_cached(diagnostic: &Diagnostic) -> CachedDiagnostic {
    CachedDiagnostic {
        code: diagnostic.code.to_string(),
        severity: severity_to_cached(diagnostic.severity),
        message: diagnostic.message.clone(),
        range_start: diagnostic.range.start().into(),
        range_end: diagnostic.range.end().into(),
    }
}

pub fn cached_to_diagnostic(cached: &CachedDiagnostic) -> Diagnostic {
    Diagnostic {
        code: static_code(&cached.code),
        severity: cached_to_severity(cached.severity),
        message: cached.message.clone(),
        range: TextRange::new(TextSize::from(cached.range_start), TextSize::from(cached.range_end)),
    }
}

pub fn param_entry_to_cached(entry: &ParamEntry) -> CachedParamEntry {
    CachedParamEntry {
        name: entry.name.clone(),
        type_description: entry.type_description.clone(),
        description: entry.description.clone(),
        range_start: entry.range.start().into(),
        range_end: entry.range.end().into(),
    }
}

pub fn cached_to_param_entry(cached: &CachedParamEntry) -> ParamEntry {
    ParamEntry {
        name: cached.name.clone(),
        type_description: cached.type_description.clone(),
        description: cached.description.clone(),
        range: TextRange::new(TextSize::from(cached.range_start), TextSize::from(cached.range_end)),
    }
}

pub fn method_views_to_cached(views: &MethodViews) -> CachedMethodViews {
    views
        .iter()
        .map(|(method, params)| {
            let cached_params: IndexMap<String, CachedParamEntry> =
                params.iter().map(|(name, entry)| (name.clone(), param_entry_to_cached(entry))).collect();
            (method.clone(), cached_params)
        })
        .collect()
}

pub fn cached_to_method_views(cached: &CachedMethodViews) -> MethodViews {
    cached
        .iter()
        .map(|(method, params)| {
            let params: IndexMap<String, ParamEntry> =
                params.iter().map(|(name, entry)| (name.clone(), cached_to_param_entry(entry))).collect();
            (method.clone(), params)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_content_sensitive() {
        assert_eq!(hash_text("hello"), hash_text("hello"));
        assert_ne!(hash_text("hello"), hash_text("world"));
    }

    #[test]
    fn global_key_ignores_module_order() {
        let a = compute_global_key(Some("numpydoc"), &["pkg.a".to_string(), "pkg.b".to_string()]);
        let b = compute_global_key(Some("numpydoc"), &["pkg.b".to_string(), "pkg.a".to_string()]);
        assert_eq!(a, b);
    }

    #[test]
    fn global_key_is_sensitive_to_style_and_module_set() {
        let base = compute_global_key(Some("numpydoc"), &["pkg.a".to_string()]);
        let different_style = compute_global_key(Some("google"), &["pkg.a".to_string()]);
        let different_modules = compute_global_key(Some("numpydoc"), &["pkg.a".to_string(), "pkg.b".to_string()]);
        assert_ne!(base, different_style);
        assert_ne!(base, different_modules);
    }

    #[test]
    fn dependency_validity_requires_every_pair_to_match() {
        let mut hashes = HashMap::new();
        hashes.insert("pkg.a".to_string(), "h1".to_string());
        hashes.insert("pkg.b".to_string(), "h2".to_string());

        assert!(dependencies_still_valid(&[("pkg.a".to_string(), "h1".to_string())], &hashes));
        assert!(!dependencies_still_valid(&[("pkg.a".to_string(), "stale".to_string())], &hashes));
        assert!(!dependencies_still_valid(&[("pkg.missing".to_string(), "h1".to_string())], &hashes));
    }

    #[test]
    fn diagnostic_round_trips_through_cache_shape() {
        let original = Diagnostic {
            code: "DOC001",
            severity: Severity::Warning,
            message: "test message".to_string(),
            range: TextRange::new(TextSize::from(3), TextSize::from(9)),
        };
        let cached = diagnostic_to_cached(&original);
        let restored = cached_to_diagnostic(&cached);
        assert_eq!(original, restored);
    }

    #[test]
    fn param_entry_round_trips_through_cache_shape() {
        let original = ParamEntry {
            name: "arg1".to_string(),
            type_description: Some("int".to_string()),
            description: Some("desc".to_string()),
            range: TextRange::new(TextSize::from(0), TextSize::from(10)),
        };
        let cached = param_entry_to_cached(&original);
        let restored = cached_to_param_entry(&cached);
        assert_eq!(original, restored);
    }
}
