//! Diagnostic and diff output formatting for the three `--output-format` modes.

use std::path::{Path, PathBuf};

use docerator_core::style::{Diagnostic, Severity};
use serde::Serialize;

pub struct FileDiagnostics<'a> {
    pub path: &'a Path,
    pub source: &'a str,
    pub diagnostics: &'a [Diagnostic],
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// One `path:line:col: CODE message` line per diagnostic — the shared core of both `text` and
/// `concise` output; `text` additionally wraps these with a summary header/footer.
fn diagnostic_lines(files: &[FileDiagnostics]) -> Vec<String> {
    let mut lines = Vec::new();
    for file in files {
        for diag in file.diagnostics {
            let (line, col) = docerator_core::location::line_column(file.source, diag.range.start());
            lines.push(format!(
                "{}:{}:{}: {} {} {}",
                display_path(file.path),
                line,
                col,
                diag.code,
                severity_label(diag.severity),
                diag.message
            ));
        }
    }
    lines
}

pub fn print_concise(files: &[FileDiagnostics]) {
    for line in diagnostic_lines(files) {
        println!("{line}");
    }
}

pub fn print_text(files: &[FileDiagnostics], changed_paths: &[PathBuf]) {
    let lines = diagnostic_lines(files);
    for line in &lines {
        println!("{line}");
    }
    let diagnostic_count = lines.len();
    let file_count = files.len();
    if !lines.is_empty() {
        println!();
    }
    println!(
        "Checked {file_count} file{}, {diagnostic_count} diagnostic{}, {} file{} would change",
        if file_count == 1 { "" } else { "s" },
        if diagnostic_count == 1 { "" } else { "s" },
        changed_paths.len(),
        if changed_paths.len() == 1 { "" } else { "s" },
    );
}

#[derive(Serialize)]
struct JsonDiagnostic<'a> {
    path: String,
    line: usize,
    column: usize,
    code: &'a str,
    severity: &'static str,
    message: &'a str,
}

#[derive(Serialize)]
struct JsonReport<'a> {
    diagnostics: Vec<JsonDiagnostic<'a>>,
    changed_files: Vec<String>,
}

pub fn print_json(files: &[FileDiagnostics], changed_paths: &[PathBuf]) {
    let mut diagnostics = Vec::new();
    for file in files {
        for diag in file.diagnostics {
            let (line, column) = docerator_core::location::line_column(file.source, diag.range.start());
            diagnostics.push(JsonDiagnostic {
                path: display_path(file.path),
                line,
                column,
                code: diag.code,
                severity: severity_label(diag.severity),
                message: &diag.message,
            });
        }
    }
    let report = JsonReport {
        diagnostics,
        changed_files: changed_paths.iter().map(|p| display_path(p)).collect(),
    };
    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("error: failed to serialize JSON report: {err}"),
    }
}

/// A unified diff of `original` -> `updated`, with `path` used as both sides' header label.
pub fn unified_diff(path: &Path, original: &str, updated: &str) -> String {
    let diff = similar::TextDiff::from_lines(original, updated);
    let label = display_path(path);
    diff.unified_diff().header(&label, &label).to_string()
}
