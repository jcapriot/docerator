use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

const BASE_PY: &str = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    arg1 : int
        Arg1 doc, from Base.
    \"\"\"

    def __init__(self, arg1):
        pass
";

const CHILD_PY_STALE: &str = "\
from .base import Base


class Child(Base):
    \"\"\"Child.

    Parameters
    ----------
    arg1 : int
        Stale text that should sync.
    \"\"\"

    def __init__(self, arg1):
        pass
";

fn write_project() -> tempfile::TempDir {
    let dir = tempdir().expect("create tempdir");
    let pkg = dir.path().join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join("__init__.py"), "").unwrap();
    fs::write(pkg.join("base.py"), BASE_PY).unwrap();
    fs::write(pkg.join("child.py"), CHILD_PY_STALE).unwrap();
    dir
}

#[test]
fn check_mode_reports_pending_change_and_does_not_write() {
    let dir = write_project();

    Command::cargo_bin("docerator")
        .unwrap()
        .arg(dir.path())
        .assert()
        .code(1)
        .stdout(predicate::str::contains("1 file would change"));

    let child_text = fs::read_to_string(dir.path().join("pkg/child.py")).unwrap();
    assert_eq!(child_text, CHILD_PY_STALE, "check mode must never write");
}

#[test]
fn fix_writes_the_regenerated_docstring_and_is_idempotent_on_rerun() {
    let dir = write_project();

    Command::cargo_bin("docerator").unwrap().args(["--fix"]).arg(dir.path()).assert().code(1);

    let child_text = fs::read_to_string(dir.path().join("pkg/child.py")).unwrap();
    assert!(child_text.contains("Arg1 doc, from Base."));
    assert!(!child_text.contains("Stale text"));

    // Second run: nothing left to change.
    Command::cargo_bin("docerator")
        .unwrap()
        .arg(dir.path())
        .assert()
        .code(0)
        .stdout(predicate::str::contains("0 files would change"));
}

#[test]
fn diff_mode_prints_a_unified_diff_and_does_not_write() {
    let dir = write_project();

    Command::cargo_bin("docerator")
        .unwrap()
        .args(["--diff"])
        .arg(dir.path())
        .assert()
        .code(1)
        .stdout(predicate::str::contains("-        Stale text that should sync."))
        .stdout(predicate::str::contains("+        Arg1 doc, from Base."));

    let child_text = fs::read_to_string(dir.path().join("pkg/child.py")).unwrap();
    assert_eq!(child_text, CHILD_PY_STALE, "diff mode must never write");
}

#[test]
fn json_output_is_well_formed_and_lists_the_changed_file() {
    let dir = write_project();

    let output = Command::cargo_bin("docerator")
        .unwrap()
        .args(["--output-format", "json"])
        .arg(dir.path())
        .output()
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let changed_files = parsed["changed_files"].as_array().expect("changed_files array");
    assert_eq!(changed_files.len(), 1);
    assert!(changed_files[0].as_str().unwrap().ends_with("child.py"));
}

#[test]
fn already_in_sync_project_exits_zero() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("solo.py"), BASE_PY).unwrap();

    Command::cargo_bin("docerator")
        .unwrap()
        .arg(dir.path())
        .assert()
        .code(0)
        .stdout(predicate::str::contains("0 files would change"));
}

#[test]
fn exit_zero_flag_forces_success_despite_pending_changes() {
    let dir = write_project();

    Command::cargo_bin("docerator").unwrap().args(["--exit-zero"]).arg(dir.path()).assert().code(0);
}

#[test]
fn cache_directory_is_created_and_self_gitignoring_by_default() {
    let dir = write_project();

    Command::cargo_bin("docerator")
        .unwrap()
        .args(["--project-root"])
        .arg(dir.path())
        .args(["--fix"])
        .arg(dir.path())
        .assert()
        .code(1);

    let cache_dir = dir.path().join(".docerator_cache");
    assert!(cache_dir.join("cache.json").exists());
    let gitignore = fs::read_to_string(cache_dir.join(".gitignore")).unwrap();
    assert_eq!(gitignore.trim(), "*");
}

#[test]
fn second_run_is_fully_cache_served_and_reports_zero_changes() {
    let dir = write_project();

    Command::cargo_bin("docerator")
        .unwrap()
        .args(["--project-root"])
        .arg(dir.path())
        .args(["--fix"])
        .arg(dir.path())
        .assert()
        .code(1);

    Command::cargo_bin("docerator")
        .unwrap()
        .args(["--project-root"])
        .arg(dir.path())
        .arg(dir.path())
        .assert()
        .code(0)
        .stdout(predicate::str::contains("0 files would change"));
}

#[test]
fn editing_only_the_ancestor_file_still_triggers_a_resync_of_the_descendant_on_next_run() {
    let dir = write_project();

    Command::cargo_bin("docerator")
        .unwrap()
        .args(["--project-root"])
        .arg(dir.path())
        .args(["--fix"])
        .arg(dir.path())
        .assert()
        .code(1);

    let updated_base = "\
class Base:
    \"\"\"Base.

    Parameters
    ----------
    arg1 : int
        UPDATED via ancestor edit.
    \"\"\"

    def __init__(self, arg1):
        pass
";
    fs::write(dir.path().join("pkg/base.py"), updated_base).unwrap();

    Command::cargo_bin("docerator")
        .unwrap()
        .args(["--project-root"])
        .arg(dir.path())
        .args(["--fix"])
        .arg(dir.path())
        .assert()
        .code(1);

    let child_text = fs::read_to_string(dir.path().join("pkg/child.py")).unwrap();
    assert!(child_text.contains("UPDATED via ancestor edit."));
}

#[test]
fn no_cache_flag_skips_creating_a_cache_directory() {
    let dir = write_project();

    Command::cargo_bin("docerator")
        .unwrap()
        .args(["--project-root"])
        .arg(dir.path())
        .args(["--no-cache", "--fix"])
        .arg(dir.path())
        .assert()
        .code(1);

    assert!(!dir.path().join(".docerator_cache").exists());
}
