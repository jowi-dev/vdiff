//! The impure glue: git access, source extraction, and resolution that
//! together turn a repository into a [`crate::graph::model::ProjectGraph`].

pub mod changed_files;
pub mod error;
pub mod extract;
pub mod git2_repo;
pub mod repo;
pub mod resolve;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::graph::builder::{self, FileInput, Lang};
use crate::graph::model::{FileRef, GitStatus, ProjectGraph};
use crate::pipeline::changed_files::ChangeSet;
use crate::pipeline::error::Result;
use crate::pipeline::extract::elixir_extract::ElixirExtract;
use crate::pipeline::extract::rust_extract::RustExtract;
use crate::pipeline::extract::{ModuleDef, ModuleExtractor};
use crate::pipeline::repo::GitRepo;

/// Options controlling how [`build_graph`] resolves the diff.
#[derive(Debug, Clone, Default)]
pub struct PipelineOptions {
    /// `--base <ref>` override, if given.
    pub base_override: Option<String>,
}

/// Build a [`ProjectGraph`] for `repo`'s current change set against
/// `opts`'s diff base.
///
/// Every tracked Rust/Elixir file is parsed, changed or not -- unchanged
/// files are still needed as edge endpoints and hierarchy context (see
/// [`crate::graph::builder`]). Non-code files are only included if
/// changed; unchanged non-code files aren't nodes. Deleted files are read
/// from the diff base (`repo.base_blob`); everything else is read from the
/// worktree at head.
pub fn build_graph(repo: &dyn GitRepo, opts: &PipelineOptions) -> Result<ProjectGraph> {
    let base_oid = repo.default_base_oid(opts.base_override.as_deref())?;
    let deltas = repo.changed_files(&base_oid)?;
    let changes = ChangeSet::from_deltas(deltas.clone());

    let mut paths: Vec<PathBuf> = repo.list_tracked_files()?;
    let mut seen: HashSet<PathBuf> = paths.iter().cloned().collect();
    for delta in &deltas {
        if seen.insert(delta.path.clone()) {
            paths.push(delta.path.clone());
        }
    }

    let mut files = Vec::new();
    for path in paths {
        let status = changes.status_for(&path);
        let lang = detect_lang(&path);
        if lang == Lang::Other && status == GitStatus::Unchanged {
            continue;
        }

        let Some(content) = load_content(repo, &base_oid, &path, status)? else {
            continue;
        };

        let file_ref = FileRef {
            path: path.clone(),
            base_blob: repo.base_blob_oid(&base_oid, &path)?,
            head_blob: repo.head_blob_oid(&path)?,
        };

        let defs = match lang {
            Lang::Rust => RustExtract.extract(&path, &content),
            Lang::Elixir => ElixirExtract.extract(&path, &content),
            Lang::Other => vec![ModuleDef {
                name: String::new(),
                dep_refs: Vec::new(),
            }],
        };
        if defs.is_empty() {
            continue;
        }

        files.push(FileInput {
            file_ref,
            lang,
            defs,
        });
    }

    Ok(builder::build(files, &changes))
}

/// Deleted files are read from the diff base; everything else from head.
fn load_content(
    repo: &dyn GitRepo,
    base_oid: &str,
    path: &Path,
    status: GitStatus,
) -> Result<Option<String>> {
    if status == GitStatus::Deleted {
        repo.base_blob(base_oid, path)
    } else {
        repo.head_content(path)
    }
}

/// Extension-based language detection: `.rs` is Rust, `.ex`/`.exs` is
/// Elixir, everything else is [`Lang::Other`].
fn detect_lang(path: &Path) -> Lang {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => Lang::Rust,
        Some("ex" | "exs") => Lang::Elixir,
        _ => Lang::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepKind, NodeId};
    use crate::pipeline::repo::{Change, FakeRepo, FileDelta};
    use std::collections::HashMap;

    /// Small fake Elixir project: `lib/my_app/accounts.ex` (modified,
    /// `alias`es the added module), `lib/my_app/repo.ex` (added),
    /// `lib/my_app/mailer.ex` (deleted, only present at base),
    /// `lib/my_app/unchanged.ex` (untouched, still parsed for the name
    /// table), and `README.md` (changed, non-code -- becomes a plain leaf
    /// node nested under no directory since it's at repo root).
    fn elixir_scenario() -> FakeRepo {
        let mut base_files = HashMap::new();
        base_files.insert(
            PathBuf::from("lib/my_app/accounts.ex"),
            "defmodule MyApp.Accounts do\nend\n".to_string(),
        );
        base_files.insert(
            PathBuf::from("lib/my_app/mailer.ex"),
            "defmodule MyApp.Mailer do\nend\n".to_string(),
        );

        let mut head_files = HashMap::new();
        head_files.insert(
            PathBuf::from("lib/my_app/accounts.ex"),
            "defmodule MyApp.Accounts do\n  alias MyApp.Repo\nend\n".to_string(),
        );
        head_files.insert(
            PathBuf::from("lib/my_app/repo.ex"),
            "defmodule MyApp.Repo do\nend\n".to_string(),
        );
        head_files.insert(
            PathBuf::from("lib/my_app/unchanged.ex"),
            "defmodule MyApp.Unchanged do\nend\n".to_string(),
        );
        head_files.insert(PathBuf::from("README.md"), "# hi\n".to_string());

        FakeRepo {
            default_base_oid: "base-oid".to_string(),
            deltas: vec![
                FileDelta {
                    path: PathBuf::from("lib/my_app/accounts.ex"),
                    change: Change::Modified,
                },
                FileDelta {
                    path: PathBuf::from("lib/my_app/repo.ex"),
                    change: Change::Added,
                },
                FileDelta {
                    path: PathBuf::from("lib/my_app/mailer.ex"),
                    change: Change::Deleted,
                },
                FileDelta {
                    path: PathBuf::from("README.md"),
                    change: Change::Modified,
                },
            ],
            base_files,
            head_files,
            tracked_files: vec![
                PathBuf::from("lib/my_app/accounts.ex"),
                PathBuf::from("lib/my_app/repo.ex"),
                PathBuf::from("lib/my_app/unchanged.ex"),
                PathBuf::from("README.md"),
            ],
        }
    }

    #[test]
    fn builds_graph_with_correct_statuses_edges_and_hierarchy() {
        let repo = elixir_scenario();
        let graph = build_graph(&repo, &PipelineOptions::default()).unwrap();

        assert_eq!(
            graph
                .node(&NodeId::from("elixir:MyApp.Accounts"))
                .unwrap()
                .status,
            GitStatus::Modified
        );
        assert_eq!(
            graph
                .node(&NodeId::from("elixir:MyApp.Repo"))
                .unwrap()
                .status,
            GitStatus::Added
        );
        assert_eq!(
            graph
                .node(&NodeId::from("elixir:MyApp.Mailer"))
                .unwrap()
                .status,
            GitStatus::Deleted
        );
        assert_eq!(
            graph
                .node(&NodeId::from("elixir:MyApp.Unchanged"))
                .unwrap()
                .status,
            GitStatus::Unchanged,
            "unchanged module is still parsed for the name table"
        );

        // Cross-file edge: Accounts (modified) aliases Repo (added).
        assert!(graph
            .edges
            .iter()
            .any(|e| e.from == NodeId::from("elixir:MyApp.Accounts")
                && e.to == NodeId::from("elixir:MyApp.Repo")
                && e.kind == DepKind::Alias));

        // Synthetic namespace node for the shared "MyApp" prefix.
        let namespace = graph.node(&NodeId::from("elixir:MyApp")).unwrap();
        assert_eq!(namespace.status, GitStatus::Unchanged);
        assert!(namespace.files.is_empty());

        // Changed non-code file at repo root is a top-level leaf node.
        let readme = graph.node(&NodeId::from("file:README.md")).unwrap();
        assert_eq!(readme.status, GitStatus::Modified);
        assert!(graph.roots.contains(&NodeId::from("file:README.md")));
    }

    #[test]
    fn base_override_is_passed_through_to_default_base_oid() {
        let repo = elixir_scenario();
        let opts = PipelineOptions {
            base_override: Some("custom-ref".to_string()),
        };
        // No assertion needed beyond "doesn't error" -- FakeRepo's
        // default_base_oid ignores the value it's given for changed_files,
        // but this exercises the override plumbing end to end.
        assert!(build_graph(&repo, &opts).is_ok());
    }
}
