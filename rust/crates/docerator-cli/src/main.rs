use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Static NumPy-docstring parameter sync for Python class hierarchies.
#[derive(Parser, Debug)]
#[command(name = "docerator")]
struct Cli {
    /// Files or directories to process (defaults to the current directory).
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let mut had_error = false;
    for path in &cli.paths {
        for entry in ignore::Walk::new(path) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    eprintln!("error: {err}");
                    had_error = true;
                    continue;
                }
            };
            if entry.path().extension().and_then(|e| e.to_str()) != Some("py") {
                continue;
            }
            let text = match fs::read_to_string(entry.path()) {
                Ok(text) => text,
                Err(err) => {
                    eprintln!("{}: {err}", entry.path().display());
                    had_error = true;
                    continue;
                }
            };
            match docerator_core::process_source(&text) {
                Ok(_) => {}
                Err(err) => {
                    eprintln!("{}: {err}", entry.path().display());
                    had_error = true;
                }
            }
        }
    }

    if had_error {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}
