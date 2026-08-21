//! Issue #15: a `--no-default-features` build has no `egui`/`eframe` at
//! all, so any invocation that would otherwise launch the GUI must exit
//! nonzero with a clear message instead of a compile hole or a hang.
//! Compiled only into the `--no-default-features` test binary (`#![cfg(not(
//! feature = "gui"))]`) -- the default-features build's GUI launch path
//! opens a real window, which this headless test suite must never do (see
//! this repo's hard rule against tests that reach `eframe::run_native`).

#![cfg(not(feature = "gui"))]

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
/// the GUI-launch branch at all rather than short-circuiting on "no
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

/// No `--dump`/`--export-comments`/`--publish-comments` at all is the
/// default GUI-launching invocation; without the `gui` feature it must
/// fail cleanly rather than panic or silently do nothing.
#[test]
fn bare_invocation_errors_cleanly_without_gui_feature() {
    let repo_dir = fixture_repo_with_changes();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .args(["--base", "main"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "expected a clean error, got: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("gui"),
        "expected the error to name the missing `gui` feature, got: {stderr}"
    );
}

/// `--smoke` (startup self-test) still tries to launch the GUI; it must
/// exit before ever reaching window creation in a headless build, which
/// this test asserts via exit code/stderr rather than by actually spawning
/// a smoke-tested window (that would violate this suite's headless-only
/// rule in a default-features build, and here there's no window to open at
/// all).
#[test]
fn smoke_flag_errors_cleanly_without_gui_feature() {
    let repo_dir = fixture_repo_with_changes();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .args(["--base", "main", "--smoke"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "expected a clean error, got: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("gui"),
        "expected the error to name the missing `gui` feature, got: {stderr}"
    );
}

/// Headless flags must keep working exactly as with the GUI feature on --
/// this is the actual point of the split: `--dump`/`--export-comments`/
/// `--publish-comments` never touch `launch_gui` at all.
#[test]
fn dump_still_works_without_gui_feature() {
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
