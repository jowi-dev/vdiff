//! End-to-end test of the default focused view / `--all` flag: runs the
//! actual built binary (`CARGO_BIN_EXE_vdiff`, cargo's standard integration-
//! test hook) against a git-init'd fixture repo and asserts on the parsed
//! `--dump json` graph's node set.
//!
//! Fixture: `MyApp.Controller` (changed) aliases `MyApp.Service`
//! (unchanged), which aliases `MyApp.Repo` (changed) -- an unchanged module
//! sitting on a dependency path between two changed modules, which the
//! default view must keep. `MyApp.Unrelated` is unchanged with no edges to
//! anything changed, and must be dropped by default but present with
//! `--all`.

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

/// `Controller` (aliases `Service`) -> `Service` (aliases `Repo`) -> `Repo`,
/// plus an `Unrelated` module with no edges. `feature` modifies `Controller`
/// and `Repo` only, leaving `Service` and `Unrelated` untouched.
fn fixture_repo() -> TempDir {
    let tmp = TempDir::new().expect("create tempdir");
    let dir = tmp.path();

    git(dir, &["init", "-b", "main"]);
    write(
        dir,
        "lib/my_app/controller.ex",
        "defmodule MyApp.Controller do\n  alias MyApp.Service\nend\n",
    );
    write(
        dir,
        "lib/my_app/service.ex",
        "defmodule MyApp.Service do\n  alias MyApp.Repo\nend\n",
    );
    write(dir, "lib/my_app/repo.ex", "defmodule MyApp.Repo do\nend\n");
    write(
        dir,
        "lib/my_app/unrelated.ex",
        "defmodule MyApp.Unrelated do\nend\n",
    );
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);

    git(dir, &["checkout", "-b", "feature"]);
    write(
        dir,
        "lib/my_app/controller.ex",
        "defmodule MyApp.Controller do\n  alias MyApp.Service\n  # touched\nend\n",
    );
    write(
        dir,
        "lib/my_app/repo.ex",
        "defmodule MyApp.Repo do\n  # touched\nend\n",
    );
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", "feature changes"]);

    tmp
}

fn dump_json_node_ids(repo_dir: &Path, extra_args: &[&str]) -> Vec<String> {
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir)
        .arg("--base")
        .arg("main")
        .args(["--dump", "json"])
        .args(extra_args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));
    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "vdiff stdout wasn't valid JSON: {err}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    value["graph"]["nodes"]
        .as_object()
        .expect("graph.nodes is an object")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn default_view_keeps_the_changed_to_changed_chain_and_drops_the_unrelated_module() {
    let repo_dir = fixture_repo();
    let ids = dump_json_node_ids(repo_dir.path(), &[]);

    assert!(ids.contains(&"elixir:MyApp.Controller".to_string()));
    assert!(
        ids.contains(&"elixir:MyApp.Service".to_string()),
        "unchanged module on a path between two changed modules must be kept: {ids:?}"
    );
    assert!(ids.contains(&"elixir:MyApp.Repo".to_string()));
    assert!(
        !ids.contains(&"elixir:MyApp.Unrelated".to_string()),
        "unrelated unchanged module must be dropped by default: {ids:?}"
    );
}

#[test]
fn all_flag_returns_the_full_unfiltered_node_set() {
    let repo_dir = fixture_repo();
    let ids = dump_json_node_ids(repo_dir.path(), &["--all"]);

    assert!(ids.contains(&"elixir:MyApp.Controller".to_string()));
    assert!(ids.contains(&"elixir:MyApp.Service".to_string()));
    assert!(ids.contains(&"elixir:MyApp.Repo".to_string()));
    assert!(
        ids.contains(&"elixir:MyApp.Unrelated".to_string()),
        "--all must show the unrelated unchanged module too: {ids:?}"
    );
}

/// `--include-diffs` only attaches diff entries for non-`Unchanged` nodes;
/// this must compose cleanly with the default filter -- every key in the
/// envelope's `diffs` map must correspond to a node actually present in the
/// (filtered) graph.
#[test]
fn include_diffs_composes_with_default_filtering() {
    let repo_dir = fixture_repo();
    let bin = env!("CARGO_BIN_EXE_vdiff");
    let output = Command::new(bin)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--base")
        .arg("main")
        .args(["--dump", "json", "--include-diffs"])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {bin}: {err}"));
    assert!(
        output.status.success(),
        "vdiff exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    let node_ids: std::collections::HashSet<String> = value["graph"]["nodes"]
        .as_object()
        .expect("graph.nodes is an object")
        .keys()
        .cloned()
        .collect();
    let diff_keys = value["diffs"].as_object().expect("diffs map present");

    for key in diff_keys.keys() {
        assert!(
            node_ids.contains(key),
            "diffs key {key} must correspond to a node present in the filtered graph"
        );
    }
    assert!(
        !node_ids.contains("elixir:MyApp.Unrelated"),
        "sanity check: unrelated module still filtered out"
    );
}
