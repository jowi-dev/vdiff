//! End-to-end fixture test: a tiny Elixir repo with real `git` (mirrors
//! `tests/dump_pipeline.rs`'s fixture pattern), where module `A`'s
//! `create/1` changed and module `B` has an unchanged `handle/1` calling
//! `A.create(x)`. Asserts [`build_function_index`] keeps both functions
//! (with the right `changed` flags) and the `B#handle/1 -> A#create/1` edge.

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;
use vdiff::graph::functions::function_node_id;
use vdiff::graph::model::NodeId;
use vdiff::pipeline::functions::build_function_index;
use vdiff::pipeline::git2_repo::Git2Repo;
use vdiff::pipeline::repo::GitRepo;
use vdiff::pipeline::{build_graph, PipelineOptions};

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

/// `lib/my_app/a.ex` (module `A`, modified: `create/1`'s body changes) and
/// `lib/my_app/b.ex` (module `B`, untouched: `handle/1` calls `A.create(x)`).
fn fixture_repo() -> TempDir {
    let tmp = TempDir::new().expect("create tempdir");
    let dir = tmp.path();

    git(dir, &["init", "-b", "main"]);

    write(
        dir,
        "lib/my_app/a.ex",
        "defmodule MyApp.A do\n  def create(x) do\n    x\n  end\nend\n",
    );
    write(
        dir,
        "lib/my_app/b.ex",
        "defmodule MyApp.B do\n  def handle(x) do\n    MyApp.A.create(x)\n  end\nend\n",
    );
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "initial"]);

    git(dir, &["checkout", "-b", "feature"]);
    write(
        dir,
        "lib/my_app/a.ex",
        "defmodule MyApp.A do\n  def create(x) do\n    x * 2\n  end\nend\n",
    );
    git(dir, &["commit", "-am", "change A.create"]);

    tmp
}

#[test]
fn keeps_changed_function_and_its_unchanged_caller_with_edge() {
    let tmp = fixture_repo();
    let repo = Git2Repo::open(tmp.path()).expect("open fixture repo");
    let opts = PipelineOptions::default();
    let base_oid = repo
        .default_base_oid(opts.base_override.as_deref())
        .unwrap();
    let graph = build_graph(&repo, &opts).expect("build_graph");

    let index = build_function_index(&repo, &graph, &base_oid).expect("build_function_index");

    let a = NodeId::from("elixir:MyApp.A");
    let b = NodeId::from("elixir:MyApp.B");

    let a_rows = index.rows_for(&a).expect("A#create/1 kept");
    assert_eq!(a_rows.len(), 1);
    assert_eq!(a_rows[0].name, "create");
    assert!(a_rows[0].changed, "A.create/1's body changed");

    let b_rows = index
        .rows_for(&b)
        .expect("B#handle/1 kept via relevant edge");
    assert_eq!(b_rows.len(), 1);
    assert_eq!(b_rows[0].name, "handle");
    assert!(
        !b_rows[0].changed,
        "B.handle/1 itself is untouched by the diff"
    );

    assert!(
        index
            .edges
            .iter()
            .any(|e| e.from == function_node_id(&b, "handle", 1)
                && e.to == function_node_id(&a, "create", 1)),
        "expected B#handle/1 -> A#create/1 edge, got {:?}",
        index.edges
    );
}
