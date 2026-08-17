//! Shared diff-loading logic for a graph node's files: read both sides of
//! each [`FileRef`] through [`GitRepo`] and diff them with [`diff_file`].
//! Used by both the diff pane ([`crate::ui::eframe_app::DiffLoader`]) and
//! the `--dump json --include-diffs` CLI payload ([`crate::cli::dump`]), so
//! the two never compute a file's diff differently.

use std::collections::HashMap;

use crate::diffing::hunks::{diff_file, FileDiff, FileDiffEntry, LinePair};
use crate::graph::model::{FileRef, GitStatus, ProjectGraph};
use crate::pipeline::repo::GitRepo;

/// Load and diff one [`FileRef`]'s content: base content from
/// [`GitRepo::base_blob`] (empty string if the file has no base blob, i.e.
/// it's new), head content from [`GitRepo::head_content`] (empty string if
/// it has no head blob, i.e. it's deleted).
pub fn load_file_diff(
    repo: &dyn GitRepo,
    base_oid: &str,
    file_ref: &FileRef,
) -> Result<FileDiff, String> {
    let base_content = if file_ref.base_blob.is_some() {
        repo.base_blob(base_oid, &file_ref.path)
            .map_err(|err| err.to_string())?
            .unwrap_or_default()
    } else {
        String::new()
    };
    let head_content = if file_ref.head_blob.is_some() {
        repo.head_content(&file_ref.path)
            .map_err(|err| err.to_string())?
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(diff_file(&base_content, &head_content))
}

/// 0-based, inclusive head-line ranges covering every hunk in `diff` that
/// touches head content -- i.e. contains at least one [`LinePair::Added`]
/// or [`LinePair::Changed`] line. A hunk that's a pure deletion (only
/// [`LinePair::Removed`]/[`LinePair::Unchanged`] lines) contributes no
/// range, since it has no head-side lines to mark. Feeds
/// [`crate::core::file_view::FileViewEntry::changed_ranges`] -- the file
/// viewer's "which lines changed" markers, computed once at load time
/// rather than the render layer walking hunks itself.
pub fn changed_head_ranges(diff: &FileDiff) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for hunk in &diff.hunks {
        let head_indices: Vec<u32> = hunk
            .lines
            .iter()
            .filter_map(|line| match *line {
                LinePair::Added { head } | LinePair::Changed { head, .. } => Some(head),
                _ => None,
            })
            .collect();
        if let (Some(&min), Some(&max)) = (head_indices.iter().min(), head_indices.iter().max()) {
            ranges.push((min as usize, max as usize));
        }
    }
    ranges
}

/// Every non-[`GitStatus::Unchanged`] node's files, diffed -- the
/// `--dump json --include-diffs` payload. Keyed by node id (stringified);
/// unchanged nodes (including synthetic ancestor nodes) are absent from the
/// map entirely, not present with an empty list.
pub fn diffs_for_graph(
    repo: &dyn GitRepo,
    base_oid: &str,
    graph: &ProjectGraph,
) -> Result<HashMap<String, Vec<FileDiffEntry>>, String> {
    let mut out = HashMap::new();
    for node in graph.nodes.values() {
        if node.status == GitStatus::Unchanged {
            continue;
        }
        let mut entries = Vec::with_capacity(node.files.len());
        for file_ref in &node.files {
            let diff = load_file_diff(repo, base_oid, file_ref)?;
            entries.push(FileDiffEntry::new(file_ref.path.clone(), diff));
        }
        out.insert(node.id.to_string(), entries);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diffing::hunks::DiffHunk;
    use crate::graph::model::{ModuleNode, NodeId};
    use crate::pipeline::repo::FakeRepo;
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;

    #[test]
    fn changed_head_ranges_covers_added_and_changed_lines_per_hunk() {
        let diff = FileDiff {
            hunks: vec![
                DiffHunk {
                    lines: vec![
                        LinePair::Unchanged { base: 0, head: 0 },
                        LinePair::Changed { base: 1, head: 1 },
                        LinePair::Added { head: 2 },
                        LinePair::Unchanged { base: 2, head: 3 },
                    ],
                },
                DiffHunk {
                    lines: vec![LinePair::Removed { base: 10 }],
                },
            ],
            base_lines: vec![],
            head_lines: vec![],
        };
        assert_eq!(
            changed_head_ranges(&diff),
            vec![(1, 2)],
            "pure-deletion hunk contributes no range"
        );
    }

    #[test]
    fn changed_head_ranges_empty_for_no_hunks() {
        let diff = FileDiff::default();
        assert_eq!(changed_head_ranges(&diff), Vec::<(usize, usize)>::new());
    }

    fn repo() -> FakeRepo {
        let mut base_files = StdHashMap::new();
        base_files.insert(PathBuf::from("changed.rs"), "before\n".to_string());
        let mut head_files = StdHashMap::new();
        head_files.insert(PathBuf::from("changed.rs"), "after\n".to_string());
        head_files.insert(PathBuf::from("new.rs"), "brand new\n".to_string());
        FakeRepo {
            default_base_oid: "base-oid".to_string(),
            deltas: vec![],
            base_files,
            head_files,
            tracked_files: vec![],
        }
    }

    fn node(id: &str, status: GitStatus, files: Vec<FileRef>) -> ModuleNode {
        ModuleNode {
            id: NodeId::from(id),
            display_name: id.to_string(),
            parent: None,
            children: vec![],
            status,
            files,
        }
    }

    #[test]
    fn diffs_for_graph_includes_only_changed_nodes() {
        let repo = repo();
        let modified = node(
            "rust:demo::changed",
            GitStatus::Modified,
            vec![FileRef {
                path: PathBuf::from("changed.rs"),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        );
        let added = node(
            "rust:demo::new",
            GitStatus::Added,
            vec![FileRef {
                path: PathBuf::from("new.rs"),
                base_blob: None,
                head_blob: Some("h2".to_string()),
            }],
        );
        let unchanged = node("rust:demo", GitStatus::Unchanged, vec![]);

        let mut nodes = StdHashMap::new();
        nodes.insert(modified.id.clone(), modified);
        nodes.insert(added.id.clone(), added);
        nodes.insert(unchanged.id.clone(), unchanged);

        let graph = ProjectGraph {
            nodes,
            roots: vec![NodeId::from("rust:demo")],
            edges: vec![],
        };

        let diffs = diffs_for_graph(&repo, "base-oid", &graph).unwrap();

        assert_eq!(diffs.len(), 2, "unchanged node must be absent");
        assert!(!diffs.contains_key("rust:demo"));

        let changed_entries = &diffs["rust:demo::changed"];
        assert_eq!(changed_entries.len(), 1);
        assert_eq!(changed_entries[0].path, PathBuf::from("changed.rs"));
        assert_eq!(changed_entries[0].base_lines, vec!["before"]);
        assert_eq!(changed_entries[0].head_lines, vec!["after"]);

        let added_entries = &diffs["rust:demo::new"];
        assert!(
            added_entries[0].base_lines.is_empty(),
            "added file has empty base"
        );
    }
}
