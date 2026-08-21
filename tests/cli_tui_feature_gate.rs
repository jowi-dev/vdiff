//! Issue #16: a `--no-default-features --features gui` (or fully headless)
//! build has no `ratatui`/`crossterm` at all, so `--tui` must exit nonzero
//! with a clear message naming the missing feature instead of a compile
//! hole or a hang. Mirrors `tests/cli_gui_feature_gate.rs` exactly, one
//! level down: that file covers the bare (GUI-launching) invocation on a
//! `not(feature = "gui")` build; this one covers `--tui` on a
//! `not(feature = "tui")` build. Compiled only into a `not(feature =
//! "tui")` test binary -- a default-features (`tui` on) build's `--tui`
//! path opens a real terminal, which this headless suite must never do.

#![cfg(not(feature = "tui"))]

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "vdiff tests")
        .env("GIT_AUTHOR_EMAIL", "vdiff-tests@example.com")
        .env("GIT_COMMITTER_NAME", "vdiff tests")
        .env("GIT_COMMITTER_EMAIL", "vdiff-tests@example.com")
        .status()
        .unwrap_or_else(|err| panic!("failed to run git {args:?}: {err}"));
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// A repo with an actual change vs `main` -- necessary so `run()` reaches
/// the `--tui` launch branch at all rather than short-circuiting on "no
/// changes vs <base>" first.
fn fixture_repo_with_changes() -> TempDir {
    let tmp = TempDir::new().expect("create tempdir");
    let dir = tmp.path();

    git(dir, &["init", "-b", "main"]);
    fs::write(dir.join("lib.rs"), "mod foo;\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);

    git(dir, &["checkout", "-b", "feature"]);
    fs::write(dir.join("lib.rs"), "mod foo;\nmod bar;\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", "touch lib"]);

    tmp
}

/// `--tui` without the `tui` feature must fail cleanly rather than panic
/// or silently launch the GUI/do nothing.
#[test]
fn tui_flag_errors_cleanly_without_tui_feature() {
    let repo_dir = fixture_repo_with_changes();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .args(["--base", "main", "--tui"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "expected a clean error, got: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("tui"),
        "expected the error to name the missing `tui` feature, got: {stderr}"
    );
}

/// `--tui --smoke` together must still fail before ever trying to touch a
/// terminal.
#[test]
fn tui_smoke_flag_errors_cleanly_without_tui_feature() {
    let repo_dir = fixture_repo_with_changes();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .args(["--base", "main", "--tui", "--smoke"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "expected a clean error, got: {stderr}"
    );
    assert!(stderr.to_lowercase().contains("tui"));
}

/// `--dump` still works headlessly with `--tui` absent, exactly as before
/// -- the point of the split (mirrors `dump_still_works_without_gui_feature`
/// in the GUI counterpart).
#[test]
fn dump_still_works_without_tui_feature() {
    let repo_dir = fixture_repo_with_changes();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .args(["--base", "main", "--dump", "text"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert!(
        output.status.success(),
        "expected --dump to succeed headlessly, got: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).is_empty());
}
