//! Smoke test for the GUI-path comment-loading glue (issue #14): running
//! `vdiff --smoke` against a fixture repo with a `comments.json` already at
//! `.git/vdiff/comments.json` should load it, map it onto the graph, and
//! reach the GUI (exit 0) same as with no store at all -- a corrupt store
//! degrades to a warning rather than a fatal error, unlike `--findings`
//! (see `tests/cli_findings.rs`), since `comments.json` is vdiff's own
//! bookkeeping rather than a hand-authored agent-contract artifact.

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

fn run_smoke(repo_dir: &Path) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_vdiff");
    Command::new(bin)
        .arg("--repo")
        .arg(repo_dir)
        .arg("--base")
        .arg("main")
        .arg("--smoke")
        .arg("--no-nvim")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"))
}

#[test]
fn smoke_with_no_comment_store_starts_clean() {
    let repo_dir = fixture_repo();
    let output = run_smoke(repo_dir.path());
    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn smoke_loads_a_valid_comment_store_without_error() {
    let repo_dir = fixture_repo();
    let comments_path = repo_dir.path().join(".git/vdiff/comments.json");
    fs::create_dir_all(comments_path.parent().unwrap()).unwrap();
    fs::write(
        &comments_path,
        r#"[
          {
            "id": "c1",
            "path": "lib.rs",
            "start_line": 1,
            "end_line": 1,
            "text": "Does this need a null check?",
            "created_at": "2026-08-18T00:00:00Z"
          }
        ]"#,
    )
    .unwrap();

    let output = run_smoke(repo_dir.path());
    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "expected no panic: {stderr}");
}

#[test]
fn smoke_degrades_gracefully_on_a_corrupt_comment_store() {
    let repo_dir = fixture_repo();
    let comments_path = repo_dir.path().join(".git/vdiff/comments.json");
    fs::create_dir_all(comments_path.parent().unwrap()).unwrap();
    fs::write(&comments_path, "not json").unwrap();

    let output = run_smoke(repo_dir.path());
    assert!(
        output.status.success(),
        "a corrupt comments.json should warn, not fail startup: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "expected no panic: {stderr}");
    assert!(
        stderr.contains("comments.json"),
        "expected a warning naming the store, got: {stderr}"
    );
}
