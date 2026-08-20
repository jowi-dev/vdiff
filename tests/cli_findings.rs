//! End-to-end tests for `vdiff --findings <path>`: the `--dump` conflict
//! and fatal load errors, both of which exit before any GUI window would
//! ever open (this test suite runs headless, so anything that reaches
//! `eframe::run_native` is out of scope here).

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

fn fixture_repo() -> TempDir {
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

#[test]
fn findings_with_dump_is_a_friendly_conflict_error() {
    let repo_dir = fixture_repo();
    let findings_path = repo_dir.path().join("findings.json");
    fs::write(&findings_path, "[]").unwrap();

    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--base")
        .arg("main")
        .args(["--dump", "text", "--findings"])
        .arg(&findings_path)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--findings") && stderr.contains("--dump"),
        "expected clap's conflict error naming both flags, got: {stderr}"
    );
}

#[test]
fn findings_file_not_found_is_a_friendly_error_not_a_panic() {
    let repo_dir = fixture_repo();
    let missing_path = repo_dir.path().join("does-not-exist.json");

    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--base")
        .arg("main")
        .arg("--smoke")
        .args(["--findings"])
        .arg(&missing_path)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "expected no panic: {stderr}");
    assert!(
        stderr.contains("does-not-exist.json"),
        "expected the bad path to be named, got: {stderr}"
    );
}

#[test]
fn findings_invalid_json_is_a_friendly_error_not_a_panic() {
    let repo_dir = fixture_repo();
    let findings_path = repo_dir.path().join("findings.json");
    fs::write(&findings_path, "not json").unwrap();

    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--base")
        .arg("main")
        .arg("--smoke")
        .args(["--findings"])
        .arg(&findings_path)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "expected no panic: {stderr}");
    assert!(
        stderr.contains("invalid findings JSON"),
        "expected a message naming the problem, got: {stderr}"
    );
}

#[test]
fn findings_entry_with_neither_node_id_nor_path_is_a_friendly_error() {
    let repo_dir = fixture_repo();
    let findings_path = repo_dir.path().join("findings.json");
    fs::write(
        &findings_path,
        r#"[{"severity":"low","summary":"anchored to nothing"}]"#,
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--base")
        .arg("main")
        .arg("--smoke")
        .args(["--findings"])
        .arg(&findings_path)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "expected no panic: {stderr}");
    assert!(
        stderr.contains("index 0"),
        "expected the failing index to be named, got: {stderr}"
    );
}
