//! `Cmd::LoadDiff`/`Cmd::LoadFile` IO for the terminal frontend: reads file
//! content through [`GitRepo`] and builds the same [`DiffPaneState`]/
//! [`FileViewState`] the GUI's `crate::ui::eframe_app::DiffLoader` builds.
//! A separate type from that one rather than a shared one -- `DiffLoader`
//! lives in `crate::ui`, which only exists behind the `gui` feature, so a
//! `--no-default-features --features tui` build (no `gui` at all) couldn't
//! reach it. The actual load logic underneath (`load_file_diff`,
//! `changed_head_ranges`) is already toolkit-free in
//! `crate::pipeline::file_diff`, so nothing here is egui-specific; it's
//! purely that `DiffLoader` the *struct* sits on the wrong side of the
//! `gui` gate for this build to use.

use crate::core::diff_state::DiffPaneState;
use crate::core::file_view::{FileViewEntry, FileViewState};
use crate::graph::model::{FileRef, GitStatus, ModuleNode, NodeId, ProjectGraph};
use crate::pipeline::file_diff::{changed_head_ranges, load_file_diff};
use crate::pipeline::repo::GitRepo;

/// Everything [`TuiLoader`] needs to read file content from git: the
/// repository and the diff base it was resolved against at startup. Mirrors
/// `crate::ui::eframe_app::DiffLoader` field-for-field.
pub struct TuiLoader {
    pub repo: Box<dyn GitRepo>,
    pub base_oid: String,
}

impl TuiLoader {
    /// Load every file backing `node` in `graph`, diffed against
    /// [`Self::base_oid`] -- backs [`crate::core::app::Cmd::LoadDiff`].
    pub fn load_diff(&self, graph: &ProjectGraph, node: &NodeId) -> Result<DiffPaneState, String> {
        let module = graph
            .node(node)
            .ok_or_else(|| format!("node {node} not found in graph"))?;

        let mut files = Vec::with_capacity(module.files.len());
        for file_ref in &module.files {
            let diff = load_file_diff(self.repo.as_ref(), &self.base_oid, file_ref)?;
            files.push(crate::core::diff_state::FileEntry {
                path: file_ref.path.clone(),
                diff,
            });
        }

        Ok(DiffPaneState::new(node.clone(), files))
    }

    /// Load the file-viewer state for every file backing `node` in `graph`
    /// -- backs [`crate::core::app::Cmd::LoadFile`]. Head content normally,
    /// falling back to base content (flagged [`FileViewEntry::deleted`])
    /// for a deleted file, exactly like the GUI's loader.
    pub fn load_file_view(
        &self,
        graph: &ProjectGraph,
        node: &NodeId,
    ) -> Result<FileViewState, String> {
        let module = graph
            .node(node)
            .ok_or_else(|| format!("node {node} not found in graph"))?;

        let mut files = Vec::with_capacity(module.files.len());
        for file_ref in &module.files {
            files.push(self.load_file_view_entry(module, file_ref)?);
        }

        Ok(FileViewState::new(node.clone(), files))
    }

    fn load_file_view_entry(
        &self,
        module: &ModuleNode,
        file_ref: &FileRef,
    ) -> Result<FileViewEntry, String> {
        let deleted = file_ref.head_blob.is_none();
        let content = if deleted {
            self.repo
                .base_blob(&self.base_oid, &file_ref.path)
                .map_err(|err| err.to_string())?
                .unwrap_or_default()
        } else {
            self.repo
                .head_content(&file_ref.path)
                .map_err(|err| err.to_string())?
                .unwrap_or_default()
        };
        let lines: Vec<String> = content.lines().map(str::to_string).collect();

        let changed_ranges = if !deleted
            && module.status != GitStatus::Unchanged
            && (file_ref.base_blob.is_some() || file_ref.head_blob.is_some())
        {
            let diff = load_file_diff(self.repo.as_ref(), &self.base_oid, file_ref)?;
            changed_head_ranges(&diff)
        } else {
            Vec::new()
        };

        Ok(FileViewEntry {
            path: file_ref.path.clone(),
            lines,
            changed_ranges,
            deleted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::repo::FakeRepo;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn graph_and_repo() -> (ProjectGraph, FakeRepo) {
        let node_id = NodeId::from("rust:demo");
        let node = ModuleNode {
            id: node_id.clone(),
            display_name: "demo".to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Modified,
            files: vec![FileRef {
                path: PathBuf::from("changed.rs"),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };
        let mut nodes = HashMap::new();
        nodes.insert(node_id.clone(), node);
        let graph = ProjectGraph {
            nodes,
            roots: vec![node_id],
            edges: vec![],
        };

        let mut base_files = HashMap::new();
        base_files.insert(PathBuf::from("changed.rs"), "before\n".to_string());
        let mut head_files = HashMap::new();
        head_files.insert(PathBuf::from("changed.rs"), "after\n".to_string());

        let repo = FakeRepo {
            default_base_oid: "base-oid".to_string(),
            deltas: vec![],
            base_files,
            head_files,
            tracked_files: vec![],
            ..Default::default()
        };
        (graph, repo)
    }

    #[test]
    fn loads_diff_state_for_a_modified_file() {
        let (graph, repo) = graph_and_repo();
        let loader = TuiLoader {
            repo: Box::new(repo),
            base_oid: "base-oid".to_string(),
        };
        let state = loader.load_diff(&graph, &NodeId::from("rust:demo")).unwrap();
        assert_eq!(state.files.len(), 1);
        assert_eq!(state.files[0].diff.base_lines, vec!["before"]);
        assert_eq!(state.files[0].diff.head_lines, vec!["after"]);
    }

    #[test]
    fn loads_file_view_with_changed_ranges() {
        let (graph, repo) = graph_and_repo();
        let loader = TuiLoader {
            repo: Box::new(repo),
            base_oid: "base-oid".to_string(),
        };
        let state = loader
            .load_file_view(&graph, &NodeId::from("rust:demo"))
            .unwrap();
        assert_eq!(state.files[0].lines, vec!["after"]);
        assert!(!state.files[0].deleted);
        assert_eq!(state.files[0].changed_ranges, vec![(0, 0)]);
    }

    #[test]
    fn errors_on_unknown_node() {
        let (graph, repo) = graph_and_repo();
        let loader = TuiLoader {
            repo: Box::new(repo),
            base_oid: "base-oid".to_string(),
        };
        assert!(loader.load_diff(&graph, &NodeId::from("nope")).is_err());
        assert!(loader
            .load_file_view(&graph, &NodeId::from("nope"))
            .is_err());
    }
}
