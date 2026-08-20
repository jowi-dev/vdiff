//! End-to-end tests of `vdiff --publish-comments <n> --dry-run`: runs the
//! actual built binary against a local fixture repo (a bare "remote" plus a
//! clone, mirroring `pipeline::pr`'s own fixture) with a `comments.json`
//! written directly at `.git/vdiff/comments.json`. No network access and no
//! real `gh` binary: `gh pr view` is stubbed by a tiny shell script placed
//! first on `PATH`, and everything else (`git fetch origin`, diffing) runs
//! against the local bare "remote", never anywhere off disk.
//!
//! `--dry-run` never calls `gh api .../reviews` (the actual POST) or `gh
//! repo view`, so the stub only needs to answer `gh pr view`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
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

/// A bare "origin" remote plus a local clone -- `gh pr view`'s
/// `baseRefName` in these tests is always `"main"`, so `resolve_pr_base_ref`
/// fetches `origin/main` from this same bare remote, no network involved.
struct Fixture {
    _remote_dir: TempDir,
    clone_dir: TempDir,
}

fn fixture_repo() -> Fixture {
    let remote_dir = TempDir::new().expect("create remote tempdir");
    git(remote_dir.path(), &["init", "--bare", "-b", "main"]);

    let seed_dir = TempDir::new().expect("create seed tempdir");
    git(seed_dir.path(), &["init", "-b", "main"]);
    write(
        seed_dir.path(),
        "src/lib.rs",
        "line one\nline two\nline three\n",
    );
    write(seed_dir.path(), "src/other.rs", "same\n");
    git(seed_dir.path(), &["add", "."]);
    git(seed_dir.path(), &["commit", "-m", "initial"]);
    git(
        seed_dir.path(),
        &["push", remote_dir.path().to_str().unwrap(), "main:main"],
    );

    let clone_dir = TempDir::new().expect("create clone tempdir");
    git(
        clone_dir.path().parent().unwrap(),
        &[
            "clone",
            remote_dir.path().to_str().unwrap(),
            clone_dir.path().to_str().unwrap(),
        ],
    );

    Fixture {
        _remote_dir: remote_dir,
        clone_dir,
    }
}

/// A stub `gh` on `PATH` answering only `gh pr view <n> --json ...` with a
/// fixed `baseRefName: "main"` -- enough for `--dry-run`, which never calls
/// `gh repo view` or posts a review.
fn stub_gh_dir() -> TempDir {
    let dir = TempDir::new().expect("create stub-gh tempdir");
    let script = dir.path().join("gh");
    fs::write(
        &script,
        "#!/bin/sh\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n\
         echo '{\"headRefName\":\"main\",\"baseRefName\":\"main\"}'\n\
         exit 0\n\
         fi\n\
         echo \"unexpected gh invocation: $@\" >&2\n\
         exit 1\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn path_with(extra_dir: &Path) -> String {
    let existing = std::env::var("PATH").unwrap_or_default();
    format!("{}:{existing}", extra_dir.display())
}

fn write_comments(repo_dir: &Path, json: &str) {
    let comments_path = repo_dir.join(".git/vdiff/comments.json");
    fs::create_dir_all(comments_path.parent().unwrap()).unwrap();
    fs::write(&comments_path, json).unwrap();
}

#[test]
fn publish_comments_with_no_store_says_nothing_to_publish_without_invoking_gh() {
    let fx = fixture_repo();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    // No `gh` anywhere on PATH at all -- if the "nothing to publish" early
    // return tried to shell out to `gh`, the process spawn itself would
    // fail with "No such file or directory", surfaced as a non-zero exit.
    let output = Command::new(bin)
        .arg("--repo")
        .arg(fx.clone_dir.path())
        .args(["--publish-comments", "1"])
        .env("PATH", "")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nothing to publish"), "stdout: {stdout}");
}

#[test]
fn publish_comments_dry_run_partitions_line_and_body_comments() {
    let fx = fixture_repo();
    // Worktree edit that's a real diff against `origin/main`: line 2 of
    // src/lib.rs changes, which is where the line-anchored comment sits.
    write(
        fx.clone_dir.path(),
        "src/lib.rs",
        "line one\nCHANGED\nline three\n",
    );
    write_comments(
        fx.clone_dir.path(),
        r#"[
          {
            "id": "c1",
            "path": "src/lib.rs",
            "start_line": 2,
            "end_line": 2,
            "text": "line comment inside the diff",
            "created_at": "2026-08-18T00:00:00Z"
          },
          {
            "id": "c2",
            "path": "src/other.rs",
            "start_line": 1,
            "end_line": 1,
            "text": "body comment on an untouched file",
            "created_at": "2026-08-18T00:01:00Z"
          }
        ]"#,
    );

    let gh_dir = stub_gh_dir();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(fx.clone_dir.path())
        .args(["--publish-comments", "1", "--dry-run"])
        .env("PATH", path_with(gh_dir.path()))
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("src/lib.rs:2"),
        "expected the diff-anchored comment as a line comment: {stdout}"
    );
    assert!(
        stdout.contains("line comment inside the diff"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Comments outside the diff"),
        "expected the untouched-file comment in the review body: {stdout}"
    );
    assert!(stdout.contains("src/other.rs:1"), "stdout: {stdout}");
    assert!(
        stdout.contains("body comment on an untouched file"),
        "stdout: {stdout}"
    );
}

#[test]
fn publish_comments_dry_run_with_no_comments_says_nothing_to_publish() {
    let fx = fixture_repo();
    let gh_dir = stub_gh_dir();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(fx.clone_dir.path())
        .args(["--publish-comments", "1", "--dry-run"])
        .env("PATH", path_with(gh_dir.path()))
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));

    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nothing to publish"), "stdout: {stdout}");
}
