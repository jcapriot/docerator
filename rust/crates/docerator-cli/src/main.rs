mod cache_io;
mod config;
mod report;
mod rules;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use docerator_core::provenance::ProvenanceMode;
use docerator_core::style::Severity;
use docerator_core::sync::{sync_project_with_cache_and_options, sync_project_with_options, SyncOptions};
use rules::RuleLevel;

/// Static NumPy-docstring parameter sync for Python class hierarchies.
#[derive(Parser, Debug)]
#[command(name = "docerator", version)]
struct Cli {
    /// Files or directories to process.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Apply edits in place.
    #[arg(long, conflicts_with = "diff")]
    fix: bool,

    /// Print a unified diff instead of writing.
    #[arg(long, conflicts_with = "fix")]
    diff: bool,

    /// Explicit check mode (the default when neither `--fix` nor `--diff` is given).
    #[arg(long)]
    check: bool,

    /// Root used for `pyproject.toml` config discovery. Defaults to the current directory.
    #[arg(long)]
    project_root: Option<PathBuf>,

    /// Additional source roots for import resolution. Reserved: cross-file resolution
    /// currently derives package roots automatically (see `docerator_core::project`); this
    /// flag is accepted but not yet consulted.
    #[arg(long = "src")]
    src: Vec<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    /// Suppress the diagnostic listing; only the final summary (or nothing, in JSON mode) prints.
    #[arg(short, long)]
    quiet: bool,

    #[arg(short, long)]
    verbose: bool,

    #[arg(long, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,

    /// Always exit 0, regardless of diagnostics or pending changes.
    #[arg(long)]
    exit_zero: bool,

    /// Directory for the on-disk incremental cache. Defaults to `<project-root>/.docerator_cache`.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Disable the on-disk cache entirely (neither read nor write it).
    #[arg(long, conflicts_with = "cache_dir")]
    no_cache: bool,

    /// Override a diagnostic code's severity, `CODE=LEVEL` (e.g. `--rule DOC001=error`).
    /// LEVEL is one of `off` (suppress entirely), `info`, `warning`, or `error`. Repeatable.
    /// Mirrors `[tool.docerator.rules]` in `pyproject.toml`; a code given both ways uses this
    /// flag's value.
    #[arg(long = "rule", value_parser = rules::parse_rule_arg)]
    rule: Vec<(String, RuleLevel)>,

    /// When a class inherits a parameter with no `Parameters` section to insert it into (DOC010),
    /// synthesize one from scratch instead of only diagnosing the gap. Mirrors
    /// `insert_missing_sections` in `[tool.docerator]`; either source turning it on is enough.
    #[arg(long)]
    insert_missing_sections: bool,

    /// Whether (and how) to make an auto-managed parameter's ancestor visible in the source: a
    /// managed comment block after the docstring, a note inline in the copied text, or neither.
    /// Defaults to `comment`. Mirrors `provenance` in `[tool.docerator]`; when both are given,
    /// this flag wins.
    #[arg(long, value_enum)]
    provenance: Option<ProvenanceModeArg>,

    /// When several consecutive, auto-managed parameters share identical documentation, render
    /// them back out as one combined `nameA, nameB : shared type` line instead of duplicating the
    /// same text once per name. Mirrors `merge_shared_parameters` in `[tool.docerator]`; either
    /// source turning it on is enough.
    #[arg(long)]
    merge_shared_parameters: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum OutputFormat {
    Text,
    Concise,
    Json,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ProvenanceModeArg {
    Off,
    Comment,
    Inline,
}

impl ProvenanceModeArg {
    fn to_core(self) -> ProvenanceMode {
        match self {
            ProvenanceModeArg::Off => ProvenanceMode::Off,
            ProvenanceModeArg::Comment => ProvenanceMode::Comment,
            ProvenanceModeArg::Inline => ProvenanceMode::Inline,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let _ = (&cli.src, cli.color); // reserved, not yet consulted

    let project_root = cli
        .project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let project_config = config::load(&project_root);

    let mut discovered: Vec<PathBuf> = Vec::new();
    for path in &cli.paths {
        for entry in ignore::Walk::new(path) {
            match entry {
                Ok(entry) => {
                    if entry.path().extension().and_then(|e| e.to_str()) == Some("py") && entry.file_type().is_some_and(|t| t.is_file()) {
                        discovered.push(entry.path().to_path_buf());
                    }
                }
                Err(err) => {
                    eprintln!("error: {err}");
                    return ExitCode::from(3);
                }
            }
        }
    }
    discovered.sort();
    discovered.dedup();

    let mut files: Vec<(PathBuf, String)> = Vec::with_capacity(discovered.len());
    for path in &discovered {
        match fs::read_to_string(path) {
            Ok(text) => files.push((path.clone(), text)),
            Err(err) => {
                eprintln!("error: {}: {err}", path.display());
                return ExitCode::from(3);
            }
        }
    }

    let cache_dir = (!cli.no_cache).then(|| cache_io::resolve_cache_dir(cli.cache_dir.as_deref(), &project_root));

    let provenance_mode = cli
        .provenance
        .map(ProvenanceModeArg::to_core)
        .or_else(|| {
            project_config.provenance.as_deref().and_then(|s| {
                s.parse::<ProvenanceMode>()
                    .inspect_err(|e| eprintln!("warning: [tool.docerator] provenance = {s:?}: {e}, ignoring"))
                    .ok()
            })
        })
        .unwrap_or_default();

    let sync_options = SyncOptions {
        project_default_style: project_config.style.as_deref(),
        insert_missing_sections: cli.insert_missing_sections || project_config.insert_missing_sections.unwrap_or(false),
        provenance_mode,
        merge_shared_parameters: cli.merge_shared_parameters || project_config.merge_shared_parameters.unwrap_or(false),
    };

    let mut outputs = if let Some(cache_dir) = &cache_dir {
        let mut cache = cache_io::load(cache_dir);
        let outputs = sync_project_with_cache_and_options(&files, sync_options, &mut cache);
        cache_io::save(cache_dir, &cache);
        outputs
    } else {
        sync_project_with_options(&files, sync_options)
    };

    // Severity overrides are a pure reporting-layer concern (never affect what edits the sync
    // engine computed above, only which diagnostics are shown and at what level) -- applied here,
    // after the cache save, so what's persisted to disk always reflects default severities and
    // stays reusable across runs with different `--rule` flags.
    let rule_overrides = rules::merge(&project_config.rules, &cli.rule);
    for output in &mut outputs {
        rules::apply(&mut output.diagnostics, &rule_overrides);
    }

    let original_by_path: std::collections::HashMap<&PathBuf, &String> = files.iter().map(|(p, t)| (p, t)).collect();

    let mut changed_paths: Vec<PathBuf> = Vec::new();
    let mut has_error_diagnostic = false;
    let mut report_files: Vec<report::FileDiagnostics> = Vec::with_capacity(outputs.len());

    for output in &outputs {
        let original = original_by_path.get(&output.path).map(|s| s.as_str()).unwrap_or_default();
        if original != output.text {
            changed_paths.push(output.path.clone());
        }
        if output.diagnostics.iter().any(|d| d.severity == Severity::Error) {
            has_error_diagnostic = true;
        }
        report_files.push(report::FileDiagnostics {
            path: &output.path,
            source: original,
            diagnostics: &output.diagnostics,
        });
    }

    if cli.fix {
        for output in &outputs {
            let original = original_by_path.get(&output.path).map(|s| s.as_str()).unwrap_or_default();
            if original != output.text {
                if let Err(err) = fs::write(&output.path, &output.text) {
                    eprintln!("error: {}: {err}", output.path.display());
                    return ExitCode::from(3);
                }
            }
        }
    } else if cli.diff {
        for output in &outputs {
            let original = original_by_path.get(&output.path).map(|s| s.as_str()).unwrap_or_default();
            if original != output.text {
                print!("{}", report::unified_diff(&output.path, original, &output.text));
            }
        }
    }

    if !cli.quiet {
        match cli.output_format {
            OutputFormat::Text => report::print_text(&report_files, &changed_paths),
            OutputFormat::Concise => report::print_concise(&report_files),
            OutputFormat::Json => report::print_json(&report_files, &changed_paths),
        }
    } else if cli.verbose {
        eprintln!("checked {} file(s)", outputs.len());
    }

    if cli.exit_zero {
        return ExitCode::SUCCESS;
    }
    if has_error_diagnostic {
        ExitCode::from(2)
    } else if !changed_paths.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
