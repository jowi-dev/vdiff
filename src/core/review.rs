//! Pure review-completion logic: which nodes count as markable ("changed"),
//! toggling a node's reviewed flag, computing "N/M changed modules
//! reviewed" progress, and invalidating a stored reviewed set against the
//! graph's current file fingerprints. No IO, no serde -- persistence (the
//! on-disk shape, load/save) lives in [`crate::review::review_state`]/
//! [`crate::review::store`]; this module only ever deals with
//! [`crate::graph::model`] types and in-memory sets.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::graph::model::{GitStatus, ModuleNode, NodeId, ProjectGraph};

/// Whether `status` counts as "changed" for review purposes: markable via
/// `v`, and counted in [`review_progress`]'s denominator. Everything except
/// [`GitStatus::Unchanged`] -- added/modified/deleted nodes all represent
/// something a reviewer actually needs to look at.
pub fn is_changed(status: GitStatus) -> bool {
    status != GitStatus::Unchanged
}

/// Toggle `id`'s presence in `reviewed`. A no-op if `id` isn't in `graph` at
/// all, or is present but [`GitStatus::Unchanged`] -- only changed nodes are
/// markable, so toggling an unchanged/test-strip node (which is never
/// focusable as a "changed" target anyway, but this stays defensive) leaves
/// `reviewed` untouched. Idempotent in the sense that two calls with the
/// same `id` cancel out: mark then unmark returns to the original set.
pub fn toggle_reviewed(reviewed: &mut HashSet<NodeId>, graph: &ProjectGraph, id: &NodeId) {
    let Some(node) = graph.node(id) else {
        return;
    };
    if !is_changed(node.status) {
        return;
    }
    if !reviewed.remove(id) {
        reviewed.insert(id.clone());
    }
}

/// "N/M changed modules reviewed": `M` is how many of `drawn` (the ids
/// actually on screen -- callers pass `App::layers` flattened, so this
/// tracks whatever `show_tests` currently has visible) are
/// [`is_changed`], and `N` is how many of those are also in `reviewed`.
/// Returns `(reviewed_count, total_count)`.
pub fn review_progress<'a>(
    graph: &ProjectGraph,
    drawn: impl Iterator<Item = &'a NodeId>,
    reviewed: &HashSet<NodeId>,
) -> (usize, usize) {
    let mut total = 0;
    let mut done = 0;
    for id in drawn {
        if let Some(node) = graph.node(id) {
            if is_changed(node.status) {
                total += 1;
                if reviewed.contains(id) {
                    done += 1;
                }
            }
        }
    }
    (done, total)
}

/// A node's file fingerprint: `(path, head_blob)` for every backing file,
/// sorted by path so the comparison in [`invalidate`] doesn't depend on
/// `files`' original order. Two nodes with the same files at the same head
/// blobs always produce equal fingerprints; a changed blob, an
/// added/removed file, or a deleted file's blob flipping to `None` all
/// produce a different one.
pub fn fingerprint(node: &ModuleNode) -> Vec<(PathBuf, Option<String>)> {
    let mut fp: Vec<(PathBuf, Option<String>)> = node
        .files
        .iter()
        .map(|f| (f.path.clone(), f.head_blob.clone()))
        .collect();
    fp.sort();
    fp
}

/// Given `stored` fingerprints (captured at mark time, keyed by node id --
/// see [`crate::review::review_state::capture`]) and the current `graph`,
/// return the subset of ids whose current [`fingerprint`] still matches
/// what was stored. A node absent from `graph` entirely (renamed away,
/// deleted, or simply out of today's change set) drops out too -- there's
/// nothing to compare against, so it can't still be "reviewed" in any
/// meaningful sense.
pub fn invalidate(
    stored: &HashMap<NodeId, Vec<(PathBuf, Option<String>)>>,
    graph: &ProjectGraph,
) -> HashSet<NodeId> {
    stored
        .iter()
        .filter(|(id, fp)| graph.node(id).is_some_and(|node| fingerprint(node) == **fp))
        .map(|(id, _)| id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::FileRef;
    use std::collections::HashMap;

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

    fn file(path: &str, head: Option<&str>) -> FileRef {
        FileRef {
            path: PathBuf::from(path),
            base_blob: Some("base".to_string()),
            head_blob: head.map(str::to_string),
        }
    }

    fn graph_with(nodes: Vec<ModuleNode>) -> ProjectGraph {
        let mut map = HashMap::new();
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
    fn is_changed_true_for_every_status_but_unchanged() {
        assert!(!is_changed(GitStatus::Unchanged));
        assert!(is_changed(GitStatus::Added));
        assert!(is_changed(GitStatus::Modified));
        assert!(is_changed(GitStatus::Deleted));
    }

    #[test]
    fn toggle_marks_a_changed_node_reviewed() {
        let graph = graph_with(vec![node(
            "a",
            GitStatus::Modified,
            vec![file("a.rs", Some("h1"))],
        )]);
        let mut reviewed = HashSet::new();
        toggle_reviewed(&mut reviewed, &graph, &NodeId::from("a"));
        assert!(reviewed.contains(&NodeId::from("a")));
    }

    #[test]
    fn toggle_is_idempotent_pairwise() {
        let graph = graph_with(vec![node(
            "a",
            GitStatus::Modified,
            vec![file("a.rs", Some("h1"))],
        )]);
        let mut reviewed = HashSet::new();
        toggle_reviewed(&mut reviewed, &graph, &NodeId::from("a"));
        toggle_reviewed(&mut reviewed, &graph, &NodeId::from("a"));
        assert!(reviewed.is_empty(), "second toggle undoes the first");
    }

    #[test]
    fn toggle_noop_on_unchanged_node() {
        let graph = graph_with(vec![node("a", GitStatus::Unchanged, vec![])]);
        let mut reviewed = HashSet::new();
        toggle_reviewed(&mut reviewed, &graph, &NodeId::from("a"));
        assert!(reviewed.is_empty());
    }

    #[test]
    fn toggle_noop_on_unknown_node() {
        let graph = graph_with(vec![]);
        let mut reviewed = HashSet::new();
        toggle_reviewed(&mut reviewed, &graph, &NodeId::from("ghost"));
        assert!(reviewed.is_empty());
    }

    #[test]
    fn review_progress_counts_only_changed_drawn_nodes() {
        let graph = graph_with(vec![
            node("a", GitStatus::Modified, vec![]),
            node("b", GitStatus::Unchanged, vec![]),
            node("c", GitStatus::Added, vec![]),
        ]);
        let mut reviewed = HashSet::new();
        reviewed.insert(NodeId::from("a"));
        let drawn = [NodeId::from("a"), NodeId::from("b"), NodeId::from("c")];
        assert_eq!(
            review_progress(&graph, drawn.iter(), &reviewed),
            (1, 2),
            "b is unchanged and doesn't count toward the denominator"
        );
    }

    #[test]
    fn review_progress_ignores_reviewed_ids_not_currently_drawn() {
        let graph = graph_with(vec![node("a", GitStatus::Modified, vec![])]);
        let mut reviewed = HashSet::new();
        reviewed.insert(NodeId::from("a"));
        reviewed.insert(NodeId::from("stale"));
        let drawn = [NodeId::from("a")];
        assert_eq!(review_progress(&graph, drawn.iter(), &reviewed), (1, 1));
    }

    #[test]
    fn fingerprint_sorts_by_path() {
        let n = node(
            "a",
            GitStatus::Modified,
            vec![file("z.rs", Some("hz")), file("a.rs", Some("ha"))],
        );
        assert_eq!(
            fingerprint(&n),
            vec![
                (PathBuf::from("a.rs"), Some("ha".to_string())),
                (PathBuf::from("z.rs"), Some("hz".to_string())),
            ]
        );
    }

    #[test]
    fn invalidate_keeps_nodes_whose_fingerprint_is_unchanged() {
        let graph = graph_with(vec![node(
            "a",
            GitStatus::Modified,
            vec![file("a.rs", Some("h1"))],
        )]);
        let mut stored = HashMap::new();
        stored.insert(
            NodeId::from("a"),
            vec![(PathBuf::from("a.rs"), Some("h1".to_string()))],
        );
        assert_eq!(
            invalidate(&stored, &graph),
            HashSet::from([NodeId::from("a")])
        );
    }

    #[test]
    fn invalidate_drops_nodes_whose_head_blob_changed() {
        let graph = graph_with(vec![node(
            "a",
            GitStatus::Modified,
            vec![file("a.rs", Some("h2"))],
        )]);
        let mut stored = HashMap::new();
        stored.insert(
            NodeId::from("a"),
            vec![(PathBuf::from("a.rs"), Some("h1".to_string()))],
        );
        assert!(invalidate(&stored, &graph).is_empty());
    }

    #[test]
    fn invalidate_drops_nodes_whose_file_set_changed() {
        let graph = graph_with(vec![node(
            "a",
            GitStatus::Modified,
            vec![file("a.rs", Some("h1")), file("b.rs", Some("h2"))],
        )]);
        let mut stored = HashMap::new();
        stored.insert(
            NodeId::from("a"),
            vec![(PathBuf::from("a.rs"), Some("h1".to_string()))],
        );
        assert!(
            invalidate(&stored, &graph).is_empty(),
            "an added file changes the fingerprint even though a.rs itself is unchanged"
        );
    }

    #[test]
    fn invalidate_drops_nodes_no_longer_in_the_graph() {
        let graph = graph_with(vec![]);
        let mut stored = HashMap::new();
        stored.insert(NodeId::from("gone"), vec![]);
        assert!(invalidate(&stored, &graph).is_empty());
    }
}
