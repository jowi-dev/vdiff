//! End-to-end test of `vdiff --export-comments`: runs the actual built
//! binary against a git-init'd fixture repo, writes a `comments.json`
//! directly at `.git/vdiff/comments.json` (mirroring what the embedded-nvim
//! capture flow's glue would have saved), and asserts on the printed
//! markdown -- headers, per-comment sections, and the empty-store case.

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

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn fixture_repo() -> TempDir {
    let tmp = TempDir::new().expect("create tempdir");
    let dir = tmp.path();

    git(dir, &["init", "-b", "review-branch"]);
    write(dir, "src/lib.rs", "fn main() {}\n");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);

    tmp
}

fn run_vdiff(repo_dir: &Path) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_vdiff");
    Command::new(bin)
        .arg("--repo")
        .arg(repo_dir)
        .arg("--export-comments")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"))
}

#[test]
fn export_comments_with_no_store_says_no_comments() {
    let repo_dir = fixture_repo();
    let output = run_vdiff(repo_dir.path());
    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No comments."), "stdout: {stdout}");
    assert!(stdout.contains("review-branch"), "stdout: {stdout}");
}

#[test]
fn export_comments_renders_stored_comments_as_markdown() {
    let repo_dir = fixture_repo();
    let comments_path = repo_dir.path().join(".git/vdiff/comments.json");
    fs::create_dir_all(comments_path.parent().unwrap()).unwrap();
    fs::write(
        &comments_path,
        r#"[
          {
            "id": "c1",
            "path": "src/lib.rs",
            "start_line": 1,
            "end_line": 1,
            "text": "Consider a doc comment here.",
            "created_at": "2026-08-18T00:00:00Z"
          },
          {
            "id": "c2",
            "path": "src/lib.rs",
            "start_line": 1,
            "end_line": 1,
            "text": "Architecture note.",
            "node": "rust:crate",
            "created_at": "2026-08-18T00:01:00Z"
          }
        ]"#,
    )
    .unwrap();

    let output = run_vdiff(repo_dir.path());
    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("### src/lib.rs:1\n"), "stdout: {stdout}");
    assert!(
        stdout.contains("### src/lib.rs:1 (node: rust:crate)\n"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("Consider a doc comment here."));
    assert!(stdout.contains("Architecture note."));
    assert!(stdout.contains("review-branch"));
}
