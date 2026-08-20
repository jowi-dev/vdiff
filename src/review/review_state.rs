//! Pure data model for the review-completion store: which nodes are marked
//! reviewed, per branch, each guarded by the file fingerprint (path + head
//! blob) it was marked under. Zero IO -- loading/saving
//! `review-state.json` lives in [`crate::review::store`]. Bridges
//! [`crate::core::review`]'s pure in-memory logic (which only ever deals in
//! [`crate::graph::model`] types) to the on-disk shape: [`capture`] turns an
//! `App::reviewed` set into a [`BranchReviewState`] ready to serialize,
//! [`seed_reviewed`] turns a loaded one back into the `HashSet<NodeId>`
//! [`crate::core::app::App::reviewed`] should start with, already run
//! through [`crate::core::review::invalidate`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::review;
use crate::graph::model::{NodeId, ProjectGraph};

/// One backing file's fingerprint at mark time: its repo-relative path and
/// head blob id (`None` for a since-deleted file). Serialized as an object
/// (`{"path": ..., "oid": ...}`) rather than a bare `(path, oid)` tuple pair
/// -- a hand-inspected `review-state.json` should read as self-explanatory
/// as `comments.json` does, not as anonymous JSON arrays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOid {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub oid: Option<String>,
}

/// Every node marked reviewed on one branch, keyed by node id (as
/// [`NodeId`]'s underlying string) to its [`FileOid`] fingerprint. A
/// [`BTreeMap`] rather than a [`HashMap`] so the serialized JSON always
/// lists nodes in the same (sorted-by-id) order regardless of insertion
/// order -- the same "stable on-disk order" property
/// [`crate::review::comments::sort_comments`] gives `comments.json`, here
/// for free from the map type instead of an explicit sort step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BranchReviewState {
    pub nodes: BTreeMap<String, Vec<FileOid>>,
}

/// The full on-disk shape of `review-state.json`: every branch's
/// [`BranchReviewState`], keyed by branch name. Keeping every branch in one
/// file (rather than one file per branch, or overwriting a single slot on
/// every branch switch) is what makes "persist per (branch, head content
/// identity)" actually work across branch switches -- checking out a
/// different branch and back doesn't lose the first branch's progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReviewStore {
    pub branches: BTreeMap<String, BranchReviewState>,
}

impl ReviewStore {
    /// `branch`'s stored state, or an empty default if this branch has
    /// never been saved before (a fresh branch with no review progress yet
    /// is not an error).
    pub fn branch(&self, branch: &str) -> BranchReviewState {
        self.branches.get(branch).cloned().unwrap_or_default()
    }

    /// Replace `branch`'s entry with `state`, leaving every other branch's
    /// entry untouched -- so saving after a toggle on one branch never
    /// clobbers progress recorded on another.
    pub fn set_branch(&mut self, branch: &str, state: BranchReviewState) {
        self.branches.insert(branch.to_string(), state);
    }
}

/// Build the [`BranchReviewState`] to persist for `reviewed`, fingerprinting
/// every id against its *current* `graph` entry (safe: within one run the
/// graph never changes, so "current" and "at mark time" are the same
/// fingerprint for every id in `reviewed`, not just the one just toggled).
/// An id in `reviewed` that's no longer in `graph` is silently dropped --
/// shouldn't happen (`core` only ever inserts ids that were in `graph` to
/// begin with), but this stays defensive rather than panicking or
/// serializing a fingerprint-less entry.
pub fn capture(graph: &ProjectGraph, reviewed: &HashSet<NodeId>) -> BranchReviewState {
    let mut nodes = BTreeMap::new();
    for id in reviewed {
        if let Some(node) = graph.node(id) {
            let fp = review::fingerprint(node)
                .into_iter()
                .map(|(path, oid)| FileOid { path, oid })
                .collect();
            nodes.insert(id.to_string(), fp);
        }
    }
    BranchReviewState { nodes }
}

/// The `HashSet<NodeId>` [`crate::core::app::App::reviewed`] should start
/// with, given `state` (loaded from disk for the current branch) and the
/// freshly-built `graph`: every stored id whose fingerprint still matches
/// (see [`crate::core::review::invalidate`]) survives; anything whose files
/// changed since it was marked -- or that dropped out of the graph
/// entirely -- doesn't.
pub fn seed_reviewed(state: &BranchReviewState, graph: &ProjectGraph) -> HashSet<NodeId> {
    let stored: HashMap<NodeId, Vec<(PathBuf, Option<String>)>> = state
        .nodes
        .iter()
        .map(|(id, fp)| {
            let fingerprint = fp.iter().map(|f| (f.path.clone(), f.oid.clone())).collect();
            (NodeId::from(id.clone()), fingerprint)
        })
        .collect();
    review::invalidate(&stored, graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{FileRef, GitStatus, ModuleNode};
    use std::collections::HashMap as StdHashMap;

    fn node(id: &str, files: Vec<FileRef>) -> ModuleNode {
        ModuleNode {
            id: NodeId::from(id),
            display_name: id.to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Modified,
            files,
        }
    }

    fn file(path: &str, head: Option<&str>) -> FileRef {
        FileRef {
            path: PathBuf::from(path),
            base_blob: Some("base".to_string()),
            head_blob: head.map(str::to_string),
        }
    }

    fn graph_with(nodes: Vec<ModuleNode>) -> ProjectGraph {
        let mut map = StdHashMap::new();
        let mut roots = Vec::new();
        for n in nodes {
            roots.push(n.id.clone());
            map.insert(n.id.clone(), n);
        }
        ProjectGraph {
            nodes: map,
            roots,
            edges: vec![],
        }
    }

    #[test]
    fn capture_then_seed_round_trips_a_reviewed_node() {
        let graph = graph_with(vec![node("a", vec![file("a.rs", Some("h1"))])]);
        let mut reviewed = HashSet::new();
        reviewed.insert(NodeId::from("a"));

        let state = capture(&graph, &reviewed);
        assert_eq!(seed_reviewed(&state, &graph), reviewed);
    }

    #[test]
    fn seed_drops_a_node_whose_head_blob_changed_since_capture() {
        let graph_then = graph_with(vec![node("a", vec![file("a.rs", Some("h1"))])]);
        let mut reviewed = HashSet::new();
        reviewed.insert(NodeId::from("a"));
        let state = capture(&graph_then, &reviewed);

        let graph_now = graph_with(vec![node("a", vec![file("a.rs", Some("h2"))])]);
        assert!(seed_reviewed(&state, &graph_now).is_empty());
    }

    #[test]
    fn capture_skips_reviewed_ids_no_longer_in_the_graph() {
        let graph = graph_with(vec![]);
        let mut reviewed = HashSet::new();
        reviewed.insert(NodeId::from("gone"));
        let state = capture(&graph, &reviewed);
        assert!(state.nodes.is_empty());
    }

    #[test]
    fn json_round_trip_preserves_shape() {
        let graph = graph_with(vec![node("a", vec![file("a.rs", Some("h1"))])]);
        let mut reviewed = HashSet::new();
        reviewed.insert(NodeId::from("a"));
        let mut store = ReviewStore::default();
        store.set_branch("main", capture(&graph, &reviewed));

        let json = serde_json::to_string_pretty(&store).expect("serialize");
        let back: ReviewStore = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, store);
    }

    #[test]
    fn set_branch_leaves_other_branches_untouched() {
        let graph = graph_with(vec![node("a", vec![file("a.rs", Some("h1"))])]);
        let mut reviewed = HashSet::new();
        reviewed.insert(NodeId::from("a"));

        let mut store = ReviewStore::default();
        store.set_branch("main", capture(&graph, &reviewed));
        store.set_branch("feature", BranchReviewState::default());

        assert!(!store.branch("main").nodes.is_empty());
        assert!(store.branch("feature").nodes.is_empty());
    }

    #[test]
    fn branch_returns_empty_default_when_never_saved() {
        let store = ReviewStore::default();
        assert!(store.branch("never-seen").nodes.is_empty());
    }

    #[test]
    fn deleted_file_omits_oid_key_and_round_trips_to_none() {
        let graph = graph_with(vec![node("a", vec![file("a.rs", None)])]);
        let mut reviewed = HashSet::new();
        reviewed.insert(NodeId::from("a"));
        let state = capture(&graph, &reviewed);
        let json = serde_json::to_string(&state).expect("serialize");
        assert!(
            !json.contains("\"oid\""),
            "oid key should be omitted: {json}"
        );
        let back: BranchReviewState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
    }
}
