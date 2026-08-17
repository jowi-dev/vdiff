//! The default "focused" view: prune a [`ProjectGraph`] down to changed
//! nodes, the nodes that connect them, and the ancestor chain needed to keep
//! the result a well-formed hierarchy. `--all` bypasses this entirely and
//! renders the raw graph -- see [`crate::main`]'s wiring.

use std::collections::{HashMap, HashSet};

use crate::graph::model::{GitStatus, ModuleNode, NodeId, ProjectGraph};

/// Prune `graph` to the nodes relevant to its change set: every node whose
/// [`GitStatus`] isn't [`GitStatus::Unchanged`], every node on a dependency
/// path between two such changed nodes, and every ancestor of a kept node
/// (so the hierarchy above a kept node -- its parent chain up to a root --
/// always survives, even if those ancestors are themselves unchanged).
///
/// If `graph` has no changed nodes at all, the result is an empty graph
/// (no nodes, no roots, no edges) -- callers render that specially (the GUI
/// prints "no changes vs `<base>`" and exits without opening a window; dump
/// mode just emits the empty graph).
pub fn focus_on_changes(graph: &ProjectGraph) -> ProjectGraph {
    let changed = changed_node_ids(graph);
    if changed.is_empty() {
        return ProjectGraph {
            nodes: HashMap::new(),
            roots: Vec::new(),
            edges: Vec::new(),
        };
    }

    let forward = reachable(graph, &changed, Direction::Forward);
    let backward = reachable(graph, &changed, Direction::Backward);
    let on_a_connecting_path: HashSet<NodeId> = forward.intersection(&backward).cloned().collect();

    let mut keep: HashSet<NodeId> = changed.into_iter().chain(on_a_connecting_path).collect();
    add_ancestors(graph, &mut keep);

    prune(graph, &keep)
}

/// Every node id whose status isn't [`GitStatus::Unchanged`].
fn changed_node_ids(graph: &ProjectGraph) -> HashSet<NodeId> {
    graph
        .nodes
        .values()
        .filter(|node| node.status != GitStatus::Unchanged)
        .map(|node| node.id.clone())
        .collect()
}

enum Direction {
    /// Follow [`crate::graph::model::DepEdge`]s from `from` to `to`.
    Forward,
    /// Follow edges from `to` back to `from`.
    Backward,
}

/// BFS from every id in `starts`, following [`crate::graph::model::DepEdge`]s
/// in `direction`. Visited-set dedup makes this safe on cyclic graphs.
fn reachable(
    graph: &ProjectGraph,
    starts: &HashSet<NodeId>,
    direction: Direction,
) -> HashSet<NodeId> {
    let adjacency = build_adjacency(graph, &direction);

    let mut visited: HashSet<NodeId> = starts.clone();
    let mut queue: Vec<NodeId> = starts.iter().cloned().collect();
    while let Some(current) = queue.pop() {
        let Some(neighbors) = adjacency.get(&current) else {
            continue;
        };
        for neighbor in neighbors {
            if visited.insert(neighbor.clone()) {
                queue.push(neighbor.clone());
            }
        }
    }
    visited
}

/// Build an adjacency map over `graph`'s edges in `direction`.
fn build_adjacency(graph: &ProjectGraph, direction: &Direction) -> HashMap<NodeId, Vec<NodeId>> {
    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for edge in &graph.edges {
        let (from, to) = match direction {
            Direction::Forward => (&edge.from, &edge.to),
            Direction::Backward => (&edge.to, &edge.from),
        };
        adjacency.entry(from.clone()).or_default().push(to.clone());
    }
    adjacency
}

/// Walk `keep`'s parent chains, adding every ancestor up to (and including)
/// each root so the pruned graph's hierarchy stays consistent.
fn add_ancestors(graph: &ProjectGraph, keep: &mut HashSet<NodeId>) {
    let mut frontier: Vec<NodeId> = keep.iter().cloned().collect();
    while let Some(id) = frontier.pop() {
        let Some(parent) = graph.node(&id).and_then(|node| node.parent.clone()) else {
            continue;
        };
        if keep.insert(parent.clone()) {
            frontier.push(parent);
        }
    }
}

/// Rebuild `graph` containing only `keep`'s nodes: edges with a dropped
/// endpoint are dropped, `children`/`roots` are filtered to kept ids
/// (preserving their existing order), and `parent` links are left as-is
/// (every kept node's ancestors are kept too, by construction). `pub(crate)`
/// so other prune-shaped filters (see [`crate::graph::test_modules`]) can
/// reuse it instead of re-implementing the same rebuild.
pub(crate) fn prune(graph: &ProjectGraph, keep: &HashSet<NodeId>) -> ProjectGraph {
    let nodes: HashMap<NodeId, ModuleNode> = graph
        .nodes
        .iter()
        .filter(|(id, _)| keep.contains(id))
        .map(|(id, node)| {
            let mut pruned = node.clone();
            pruned.children.retain(|child| keep.contains(child));
            (id.clone(), pruned)
        })
        .collect();

    let roots: Vec<NodeId> = graph
        .roots
        .iter()
        .filter(|id| keep.contains(*id))
        .cloned()
        .collect();

    let edges = graph
        .edges
        .iter()
        .filter(|edge| keep.contains(&edge.from) && keep.contains(&edge.to))
        .cloned()
        .collect();

    ProjectGraph {
        nodes,
        roots,
        edges,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepEdge, DepKind, FileRef};
    use std::path::PathBuf;

    /// Build a node with the given id/status/parent/children, no files.
    fn node(id: &str, status: GitStatus, parent: Option<&str>, children: &[&str]) -> ModuleNode {
        ModuleNode {
            id: NodeId::from(id),
            display_name: id.to_string(),
            parent: parent.map(NodeId::from),
            children: children.iter().map(|c| NodeId::from(*c)).collect(),
            status,
            files: vec![FileRef {
                path: PathBuf::from(format!("{id}.rs")),
                base_blob: Some("base".to_string()),
                head_blob: Some("head".to_string()),
            }],
        }
    }

    fn edge(from: &str, to: &str) -> DepEdge {
        DepEdge {
            from: NodeId::from(from),
            to: NodeId::from(to),
            kind: DepKind::Use,
        }
    }

    fn graph(nodes: Vec<ModuleNode>, roots: &[&str], edges: Vec<DepEdge>) -> ProjectGraph {
        ProjectGraph {
            nodes: nodes.into_iter().map(|n| (n.id.clone(), n)).collect(),
            roots: roots.iter().map(|r| NodeId::from(*r)).collect(),
            edges,
        }
    }

    /// The user's own example: view (Modified) -> moduleA (Unchanged) ->
    /// moduleB (Modified). All three are kept; moduleA stays gray
    /// (Unchanged) since only its *status* determines color, not the fact
    /// it's on a connecting path.
    #[test]
    fn keeps_unchanged_node_on_a_path_between_two_changed_nodes() {
        let g = graph(
            vec![
                node("view", GitStatus::Modified, None, &["moduleA"]),
                node("moduleA", GitStatus::Unchanged, Some("view"), &["moduleB"]),
                node("moduleB", GitStatus::Modified, Some("moduleA"), &[]),
            ],
            &["view"],
            vec![edge("view", "moduleA"), edge("moduleA", "moduleB")],
        );

        let focused = focus_on_changes(&g);

        assert!(focused.node(&NodeId::from("view")).is_some());
        let module_a = focused
            .node(&NodeId::from("moduleA"))
            .expect("moduleA kept");
        assert_eq!(module_a.status, GitStatus::Unchanged, "moduleA stays gray");
        assert!(focused.node(&NodeId::from("moduleB")).is_some());
    }

    /// An unchanged node reachable *from* a changed node but with no path
    /// onward to any other changed node (a dead-end dependency) is dropped:
    /// it's forward-reachable but not backward-reachable from the changed
    /// set, so it fails the intersection test.
    #[test]
    fn drops_dead_end_dependency_of_a_changed_node() {
        let g = graph(
            vec![
                node("changed", GitStatus::Modified, None, &["dead_end"]),
                node("dead_end", GitStatus::Unchanged, Some("changed"), &[]),
            ],
            &["changed"],
            vec![edge("changed", "dead_end")],
        );

        let focused = focus_on_changes(&g);

        assert!(focused.node(&NodeId::from("changed")).is_some());
        assert!(
            focused.node(&NodeId::from("dead_end")).is_none(),
            "dead-end dependency with no path onward to another changed node must be dropped"
        );
    }

    /// An unchanged leaf with no edges at all is dropped, and its unchanged
    /// parent is dropped too when it has no other kept descendants.
    #[test]
    fn drops_unchanged_leaf_and_its_childless_unchanged_parent() {
        let g = graph(
            vec![
                node("root", GitStatus::Unchanged, None, &["leaf"]),
                node("leaf", GitStatus::Unchanged, Some("root"), &[]),
            ],
            &["root"],
            vec![],
        );

        let focused = focus_on_changes(&g);

        assert!(focused.nodes.is_empty());
        assert!(focused.roots.is_empty());
    }

    /// An unchanged parent IS kept when it has a kept (changed) child --
    /// the ancestor rule keeps hierarchy context above any kept node.
    #[test]
    fn keeps_unchanged_ancestor_of_a_changed_node() {
        let g = graph(
            vec![
                node("root", GitStatus::Unchanged, None, &["child"]),
                node("child", GitStatus::Modified, Some("root"), &[]),
            ],
            &["root"],
            vec![],
        );

        let focused = focus_on_changes(&g);

        let root = focused
            .node(&NodeId::from("root"))
            .expect("root kept as ancestor");
        assert_eq!(root.status, GitStatus::Unchanged);
        assert_eq!(root.children, vec![NodeId::from("child")]);
        assert_eq!(focused.roots, vec![NodeId::from("root")]);
    }

    /// Edges to dropped nodes are removed; children/roots are pruned but
    /// their remaining order is preserved.
    #[test]
    fn prunes_edges_children_and_roots_preserving_order() {
        let g = graph(
            vec![
                node(
                    "root",
                    GitStatus::Unchanged,
                    None,
                    &["zeta", "changed", "dead"],
                ),
                node("zeta", GitStatus::Unchanged, Some("root"), &[]),
                node("changed", GitStatus::Modified, Some("root"), &[]),
                node("dead", GitStatus::Unchanged, Some("root"), &[]),
            ],
            &["dead", "root"],
            vec![edge("changed", "dead"), edge("root", "changed")],
        );

        let focused = focus_on_changes(&g);

        assert!(focused.node(&NodeId::from("zeta")).is_none());
        assert!(focused.node(&NodeId::from("dead")).is_none());
        let root = focused.node(&NodeId::from("root")).unwrap();
        assert_eq!(root.children, vec![NodeId::from("changed")]);
        assert_eq!(focused.roots, vec![NodeId::from("root")]);
        assert!(focused
            .edges
            .iter()
            .all(|e| e.from != NodeId::from("changed") || e.to != NodeId::from("dead")));
    }

    /// Zero changed nodes yields an empty graph.
    #[test]
    fn no_changed_nodes_yields_empty_graph() {
        let g = graph(
            vec![node("only", GitStatus::Unchanged, None, &[])],
            &["only"],
            vec![],
        );

        let focused = focus_on_changes(&g);

        assert!(focused.nodes.is_empty());
        assert!(focused.roots.is_empty());
        assert!(focused.edges.is_empty());
    }

    /// Cycle safety: `changed` -> a -> b -> a (a cycle between a and b),
    /// with b also depending on a second changed node `changed2`. The cycle
    /// must not hang BFS, and a/b must be kept: both sit on a path from
    /// `changed` to `changed2` (forward-reachable from `changed`,
    /// backward-reachable to `changed2`), even though reaching that
    /// forward/backward status requires looping through the a<->b cycle.
    #[test]
    fn terminates_on_a_cycle_and_keeps_nodes_on_a_changed_to_changed_path() {
        let g = graph(
            vec![
                node("changed", GitStatus::Modified, None, &["a"]),
                node("a", GitStatus::Unchanged, Some("changed"), &["b"]),
                node("b", GitStatus::Unchanged, Some("a"), &["changed2"]),
                node("changed2", GitStatus::Modified, Some("b"), &[]),
            ],
            &["changed"],
            vec![
                edge("changed", "a"),
                edge("a", "b"),
                edge("b", "a"),
                edge("b", "changed2"),
            ],
        );

        let focused = focus_on_changes(&g);

        assert!(focused.node(&NodeId::from("changed")).is_some());
        assert!(focused.node(&NodeId::from("a")).is_some());
        assert!(focused.node(&NodeId::from("b")).is_some());
        assert!(focused.node(&NodeId::from("changed2")).is_some());
    }
}
