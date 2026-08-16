//! End-to-end test of `vdiff --dump json --include-diffs`: runs the actual
//! built binary (via `CARGO_BIN_EXE_vdiff`, cargo's standard integration-test
//! hook) against a git-init'd fixture repo and asserts on the parsed JSON
//! envelope -- the `diffs` map, not just the graph.

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

    git(dir, &["init", "-b", "main"]);
    write(dir, "backend/src/lib.rs", "mod foo;\n");
    write(dir, "backend/src/foo.rs", "pub fn helper() {}\n");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);

    git(dir, &["checkout", "-b", "feature"]);
    write(
        dir,
        "backend/src/foo.rs",
        "pub fn helper() {}\npub fn other() {}\n",
    );
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", "touch foo"]);

    tmp
}

fn run_vdiff(repo_dir: &Path, extra_args: &[&str]) -> serde_json::Value {
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir)
        .arg("--base")
        .arg("main")
        .args(extra_args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));
    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "vdiff stdout wasn't valid JSON: {err}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn dump_json_without_include_diffs_has_no_diffs_key() {
    let repo_dir = fixture_repo();
    let value = run_vdiff(repo_dir.path(), &["--dump", "json"]);
    let obj = value.as_object().unwrap();
    assert!(obj.contains_key("graph"));
    assert!(!obj.contains_key("diffs"));
}

#[test]
fn dump_json_include_diffs_puts_modified_files_hunks_under_their_node_id() {
    let repo_dir = fixture_repo();
    let value = run_vdiff(repo_dir.path(), &["--dump", "json", "--include-diffs"]);
    let obj = value.as_object().unwrap();
    assert!(obj.contains_key("graph"));

    let diffs = obj["diffs"].as_object().expect("diffs map present");
    let foo_entries = diffs["rust:backend::foo"]
        .as_array()
        .expect("rust:backend::foo has diff entries");
    assert_eq!(foo_entries.len(), 1);
    assert_eq!(foo_entries[0]["path"], "backend/src/foo.rs");
    assert!(
        !foo_entries[0]["hunks"].as_array().unwrap().is_empty(),
        "modified file must have at least one hunk"
    );

    // lib.rs wasn't touched between main and feature, so its node (also the
    // crate root) must be entirely absent from the diffs map.
    assert!(
        !diffs.contains_key("rust:backend"),
        "unchanged node must not appear in the diffs map"
    );
}

#[test]
fn include_diffs_with_dump_text_is_a_friendly_error() {
    let repo_dir = fixture_repo();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--base")
        .arg("main")
        .args(["--dump", "text", "--include-diffs"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--include-diffs") && stderr.contains("--dump json"),
        "expected a friendly error naming both flags, got: {stderr}"
    );
}
