//! Reads/writes the on-disk cache file (`docerator_core::cache::Cache`) — the CLI owns *where*
//! the cache lives and *when* it's persisted; the actual incrementality logic lives in
//! `docerator_core::sync::sync_project_with_cache`.

use std::fs;
use std::path::{Path, PathBuf};

use docerator_core::cache::Cache;

const CACHE_FILE_NAME: &str = "cache.json";

/// `--cache-dir` if given, otherwise `<project_root>/.docerator_cache`.
pub fn resolve_cache_dir(explicit: Option<&Path>, project_root: &Path) -> PathBuf {
    explicit.map(Path::to_path_buf).unwrap_or_else(|| project_root.join(".docerator_cache"))
}

/// Loads the cache file if present and parses cleanly; anything else (missing, unreadable,
/// malformed JSON) is treated as "no cache yet" rather than an error — the cache is a pure
/// performance optimization, never required for correctness.
pub fn load(cache_dir: &Path) -> Cache {
    let path = cache_dir.join(CACHE_FILE_NAME);
    let Ok(text) = fs::read_to_string(&path) else {
        return Cache::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persists the cache, creating the directory (and a one-time `.gitignore` inside it, matching
/// the convention tools like ruff's own `.ruff_cache/` use) if needed. Failures are reported but
/// non-fatal — losing the cache only costs speed on the next run, never correctness.
pub fn save(cache_dir: &Path, cache: &Cache) {
    if let Err(err) = fs::create_dir_all(cache_dir) {
        eprintln!("warning: failed to create cache directory {}: {err}", cache_dir.display());
        return;
    }

    let gitignore = cache_dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = fs::write(&gitignore, "*\n");
    }

    let path = cache_dir.join(CACHE_FILE_NAME);
    match serde_json::to_string(cache) {
        Ok(json) => {
            if let Err(err) = fs::write(&path, json) {
                eprintln!("warning: failed to write cache file {}: {err}", path.display());
            }
        }
        Err(err) => eprintln!("warning: failed to serialize cache: {err}"),
    }
}
