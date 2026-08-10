mod cache_io;
mod config;
mod report;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use docerator_core::style::Severity;
use docerator_core::sync::{sync_project, sync_project_with_cache};

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

    let outputs = if let Some(cache_dir) = &cache_dir {
        let mut cache = cache_io::load(cache_dir);
        let outputs = sync_project_with_cache(&files, project_config.style.as_deref(), &mut cache);
        cache_io::save(cache_dir, &cache);
        outputs
    } else {
        sync_project(&files, project_config.style.as_deref())
    };
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
