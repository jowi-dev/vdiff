//! End-to-end fixture tests: git-init a tiny mixed Rust/Elixir repo with
//! real `git` (available in the nix devShell), commit on `main`, branch,
//! make changes, then run the real pipeline (`Git2Repo` + `build_graph`)
//! against the branch checkout and assert on the parsed `--dump json`
//! output.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;
use vdiff::graph::model::{DepKind, GitStatus, NodeId, ProjectGraph};
use vdiff::pipeline::git2_repo::Git2Repo;
use vdiff::pipeline::{build_graph, PipelineOptions};

/// Run a git command in `dir`, with committer/author env vars set so this
/// works in CI-like environments with no configured git identity, and
/// commit signing disabled for this invocation only (a fixture repo, not
/// the project's own history).
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

fn remove(dir: &Path, rel: &str) {
    fs::remove_file(dir.join(rel)).unwrap();
}

/// Build the fixture repo: a tiny Rust crate (`backend/src/{lib,foo}.rs`)
/// and a tiny Elixir project (`lib/my_app/{repo,accounts}.ex`), committed
/// on `main`, then a `feature` branch that modifies `accounts.ex` (adding
/// an `alias` on `MyApp.Repo`), adds `lib/my_app/mailer.ex`, deletes
/// `backend/src/foo.rs`, and modifies `README.md`.
fn fixture_repo() -> TempDir {
    let tmp = TempDir::new().expect("create tempdir");
    let dir = tmp.path();

    git(dir, &["init", "-b", "main"]);

    write(dir, "backend/src/lib.rs", "mod foo;\n\npub fn entry() {}\n");
    write(dir, "backend/src/foo.rs", "pub fn helper() {}\n");
    write(dir, "lib/my_app/repo.ex", "defmodule MyApp.Repo do\nend\n");
    write(
        dir,
        "lib/my_app/accounts.ex",
        "defmodule MyApp.Accounts do\nend\n",
    );
    write(dir, "README.md", "# hi\n");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);

    git(dir, &["checkout", "-b", "feature"]);
    write(
        dir,
        "lib/my_app/accounts.ex",
        "defmodule MyApp.Accounts do\n  alias MyApp.Repo\nend\n",
    );
    write(
        dir,
        "lib/my_app/mailer.ex",
        "defmodule MyApp.Mailer do\nend\n",
    );
    remove(dir, "backend/src/foo.rs");
    write(dir, "README.md", "# hi\n\nnow with more detail.\n");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", "feature changes"]);

    tmp
}

fn dump_json(repo_dir: &Path, base_override: Option<&str>) -> ProjectGraph {
    let repo = Git2Repo::open(repo_dir).expect("open fixture repo");
    let opts = PipelineOptions {
        base_override: base_override.map(str::to_string),
    };
    build_graph(&repo, &opts).expect("build_graph")
}

fn assert_fixture_graph(graph: &ProjectGraph) {
    assert_eq!(
        graph
            .node(&NodeId::from("elixir:MyApp.Accounts"))
            .expect("Accounts node")
            .status,
        GitStatus::Modified
    );
    assert_eq!(
        graph
            .node(&NodeId::from("elixir:MyApp.Repo"))
            .expect("Repo node")
            .status,
        GitStatus::Unchanged,
        "Repo.ex wasn't touched between main and feature"
    );
    assert_eq!(
        graph
            .node(&NodeId::from("elixir:MyApp.Mailer"))
            .expect("Mailer node")
            .status,
        GitStatus::Added
    );

    let namespace = graph
        .node(&NodeId::from("elixir:MyApp"))
        .expect("synthesized MyApp namespace node");
    assert_eq!(namespace.status, GitStatus::Unchanged);
    assert!(namespace.files.is_empty());

    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.from == NodeId::from("elixir:MyApp.Accounts")
                && e.to == NodeId::from("elixir:MyApp.Repo")
                && e.kind == DepKind::Alias),
        "expected an Alias edge from MyApp.Accounts to MyApp.Repo, got {:?}",
        graph.edges
    );

    let backend_foo = graph
        .node(&NodeId::from("rust:backend::foo"))
        .expect("backend::foo node (read from base blob since deleted)");
    assert_eq!(backend_foo.status, GitStatus::Deleted);

    assert_eq!(
        graph
            .node(&NodeId::from("rust:backend"))
            .expect("backend crate root")
            .status,
        GitStatus::Unchanged,
        "lib.rs wasn't touched between main and feature"
    );

    let readme = graph
        .node(&NodeId::from("file:README.md"))
        .expect("README.md node");
    assert_eq!(readme.status, GitStatus::Modified);
    assert!(graph.roots.contains(&NodeId::from("file:README.md")));
}

#[test]
fn dumps_correct_statuses_edges_and_hierarchy_with_default_base() {
    let repo_dir = fixture_repo();
    let graph = dump_json(repo_dir.path(), None);
    assert_fixture_graph(&graph);
}

#[test]
fn base_override_produces_the_same_result_as_default_detection() {
    let repo_dir = fixture_repo();
    let graph = dump_json(repo_dir.path(), Some("main"));
    assert_fixture_graph(&graph);
}
