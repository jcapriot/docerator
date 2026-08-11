//! Per-diagnostic-code severity configuration — `[tool.docerator.rules]` in `pyproject.toml` and
//! the repeatable `--rule CODE=LEVEL` CLI flag both resolve into one override map, applied
//! uniformly over every file's diagnostics after `sync_project`/`sync_project_with_cache`
//! returns. Severity and suppression are purely a reporting concern here — applying an override
//! never changes what edits the sync engine computes (see `docerator_core::sync`); it only
//! changes which diagnostics are shown, and at what level. That's why this lives entirely in the
//! CLI crate as a post-processing pass, rather than threading config into the engine itself.

use std::collections::HashMap;
use std::str::FromStr;

use docerator_core::style::{Diagnostic, Severity, KNOWN_DIAGNOSTIC_CODES};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleLevel {
    Off,
    Info,
    Warning,
    Error,
}

impl RuleLevel {
    fn to_severity(self) -> Option<Severity> {
        match self {
            RuleLevel::Off => None,
            RuleLevel::Info => Some(Severity::Info),
            RuleLevel::Warning => Some(Severity::Warning),
            RuleLevel::Error => Some(Severity::Error),
        }
    }
}

impl FromStr for RuleLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(RuleLevel::Off),
            "info" => Ok(RuleLevel::Info),
            "warning" | "warn" => Ok(RuleLevel::Warning),
            "error" => Ok(RuleLevel::Error),
            other => Err(format!("'{other}' is not a valid rule level (expected one of: off, info, warning, error)")),
        }
    }
}

/// Parses one `--rule` flag value, `CODE=LEVEL` (e.g. `DOC001=error`) — used directly as clap's
/// `value_parser`, so a malformed flag (missing `=`, unrecognized level word) is rejected
/// immediately as an ordinary CLI usage error, the same way clap rejects any other malformed
/// argument. That's distinct from an unrecognized-but-well-formed *code* (e.g. `DOC099=error`),
/// which is a configuration nuance rather than a syntax error and is validated separately, in
/// `merge` below, as a warning rather than a hard failure.
pub fn parse_rule_arg(s: &str) -> Result<(String, RuleLevel), String> {
    let (code, level) = s.split_once('=').ok_or_else(|| format!("expected CODE=LEVEL (e.g. DOC001=error), got '{s}'"))?;
    let level: RuleLevel = level.parse()?;
    Ok((code.trim().to_ascii_uppercase(), level))
}

/// The effective set of per-code overrides after merging `[tool.docerator.rules]` (from
/// `pyproject.toml`) with `--rule` (CLI) — CLI wins wherever both mention the same code, matching
/// every other layered setting in this tool: entity directive wins over file directive, which
/// wins over project config, which wins over the hardcoded default (see `docerator_core::sync`'s
/// style resolution for the established precedent this mirrors). An unrecognized code from
/// either source warns and is otherwise harmless (it simply never matches any real diagnostic's
/// `code`), mirroring how an unknown `# docerator:` directive key is diagnosed but doesn't stop
/// the entity from being processed.
pub fn merge(pyproject_rules: &HashMap<String, String>, cli_rules: &[(String, RuleLevel)]) -> HashMap<String, RuleLevel> {
    let mut overrides = HashMap::new();
    for (code, level) in pyproject_rules {
        match level.parse::<RuleLevel>() {
            Ok(level) => {
                let code = code.trim().to_ascii_uppercase();
                warn_if_unknown(&code);
                overrides.insert(code, level);
            }
            Err(err) => {
                eprintln!("warning: [tool.docerator.rules] '{code}': {err}, ignoring");
            }
        }
    }
    for (code, level) in cli_rules {
        warn_if_unknown(code);
        overrides.insert(code.clone(), *level);
    }
    overrides
}

fn warn_if_unknown(code: &str) {
    if !KNOWN_DIAGNOSTIC_CODES.iter().any(|&(known, _)| known == code) {
        eprintln!("warning: '{code}' is not a known diagnostic code, ignoring its rule override");
    }
}

/// Applies `overrides` to `diagnostics` in place: a code mapped to `Off` is dropped entirely; any
/// other level replaces that diagnostic's severity. A code with no override keeps its default
/// severity untouched.
pub fn apply(diagnostics: &mut Vec<Diagnostic>, overrides: &HashMap<String, RuleLevel>) {
    diagnostics.retain_mut(|d| match overrides.get(d.code) {
        Some(level) => match level.to_severity() {
            Some(severity) => {
                d.severity = severity;
                true
            }
            None => false,
        },
        None => true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_rule_args() {
        assert_eq!(parse_rule_arg("DOC001=error"), Ok(("DOC001".to_string(), RuleLevel::Error)));
        assert_eq!(parse_rule_arg("doc002=off"), Ok(("DOC002".to_string(), RuleLevel::Off)));
        assert_eq!(parse_rule_arg("DOC003=warn"), Ok(("DOC003".to_string(), RuleLevel::Warning)));
    }

    #[test]
    fn rejects_malformed_rule_args() {
        assert!(parse_rule_arg("DOC001").is_err());
        assert!(parse_rule_arg("DOC001=critical").is_err());
    }

    #[test]
    fn cli_override_wins_over_pyproject_for_the_same_code() {
        let pyproject = HashMap::from([("DOC001".to_string(), "warning".to_string())]);
        let cli = vec![("DOC001".to_string(), RuleLevel::Error)];
        let merged = merge(&pyproject, &cli);
        assert_eq!(merged.get("DOC001"), Some(&RuleLevel::Error));
    }

    #[test]
    fn off_drops_the_diagnostic_and_other_levels_replace_severity() {
        let mut diagnostics = vec![
            Diagnostic {
                code: "DOC001",
                severity: Severity::Warning,
                message: "m".to_string(),
                range: Default::default(),
            },
            Diagnostic {
                code: "DOC002",
                severity: Severity::Info,
                message: "m".to_string(),
                range: Default::default(),
            },
        ];
        let overrides = HashMap::from([("DOC001".to_string(), RuleLevel::Off), ("DOC002".to_string(), RuleLevel::Error)]);
        apply(&mut diagnostics, &overrides);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "DOC002");
        assert_eq!(diagnostics[0].severity, Severity::Error);
    }
}
