//! End-to-end friendly-error tests: running `vdiff` against inputs that
//! can't produce a graph must exit 1 with a clear one-line message, never a
//! panic/backtrace.

use std::process::Command;

use tempfile::TempDir;

/// Running `vdiff` against a directory with no `.git` anywhere above it
/// must exit 1 with a clear one-line message, never a panic/backtrace.
#[test]
fn running_outside_a_git_repo_is_a_friendly_error_not_a_panic() {
    let tmp = TempDir::new().expect("create tempdir");
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(tmp.path())
        .args(["--dump", "text"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked") && !stderr.contains("RUST_BACKTRACE"),
        "expected a clean error, got a panic: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("not a git repository"),
        "expected a message naming the problem, got: {stderr}"
    );
    assert_eq!(
        stderr.lines().count(),
        1,
        "expected a single-line error, got: {stderr}"
    );
}

/// `--base nonexistent-ref` against a real repo must exit 1 naming the ref,
/// not a raw git2 error dump.
#[test]
fn nonexistent_base_ref_is_a_friendly_error() {
    let tmp = TempDir::new().expect("create tempdir");
    let dir = tmp.path();
    let git = |args: &[&str]| {
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
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "initial"]);

    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(dir)
        .args(["--base", "nonexistent-ref", "--dump", "text"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"));
    assert!(
        stderr.contains("nonexistent-ref"),
        "expected the bad ref to be named, got: {stderr}"
    );
}
