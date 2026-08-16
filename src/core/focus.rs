//! h/j/k/l tree-walk navigation over a [`ProjectGraph`]. Pure: consults only
//! the graph's parent/children edges, never layout geometry.

use crate::graph::model::{NodeId, ProjectGraph};

/// A single navigation step, named after its vim keybinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `h`: ascend to the parent. No-op on a root.
    Left,
    /// `j`: move to the next sibling. No-op on the last sibling.
    Down,
    /// `k`: move to the previous sibling. No-op on the first sibling.
    Up,
    /// `l`: descend to the first child. No-op on a leaf.
    Right,
}

/// Move focus from `current` in `dir`, returning the new focused node.
/// Returns `current` unchanged for every no-op case documented on
/// [`Direction`]'s variants. Sibling order is [`ProjectGraph::sorted_children`]
/// (or [`ProjectGraph::sorted_roots`] for top-level nodes) -- the same order
/// the graph view renders.
pub fn move_focus(graph: &ProjectGraph, current: &NodeId, dir: Direction) -> NodeId {
    match dir {
        Direction::Left => parent_of(graph, current).unwrap_or_else(|| current.clone()),
        Direction::Right => first_child(graph, current).unwrap_or_else(|| current.clone()),
        Direction::Down => sibling_step(graph, current, 1).unwrap_or_else(|| current.clone()),
        Direction::Up => sibling_step(graph, current, -1).unwrap_or_else(|| current.clone()),
    }
}

/// `current`'s parent, if it has one and it resolves to a real node.
fn parent_of(graph: &ProjectGraph, current: &NodeId) -> Option<NodeId> {
    graph.node(current)?.parent.clone()
}

/// The first (name-sorted) child of `current`, if it has any.
fn first_child(graph: &ProjectGraph, current: &NodeId) -> Option<NodeId> {
    graph.sorted_children(current).into_iter().next()
}

/// Step `delta` positions through `current`'s sibling list (root list if
/// `current` has no parent), clamped to the list's bounds. `None` if the
/// step would be a no-op (already at that end) or `current` isn't found in
/// its own sibling list (shouldn't happen for a well-formed graph).
fn sibling_step(graph: &ProjectGraph, current: &NodeId, delta: i32) -> Option<NodeId> {
    let siblings = match parent_of(graph, current) {
        Some(parent) => graph.sorted_children(&parent),
        None => graph.sorted_roots(),
    };
    let index = siblings.iter().position(|id| id == current)?;
    let new_index = index as i32 + delta;
    if new_index < 0 || new_index as usize >= siblings.len() {
        return None;
    }
    siblings.get(new_index as usize).cloned()
}

/// Outgoing dependency targets of `node`: the `to` end of every edge whose
/// `from` is `node`, deduped and sorted by `display_name`. Empty if `node`
/// has no outgoing edges.
pub fn dep_targets(graph: &ProjectGraph, node: &NodeId) -> Vec<NodeId> {
    let targets = graph
        .edges
        .iter()
        .filter(|edge| &edge.from == node)
        .map(|edge| edge.to.clone());
    dedup_sorted_by_name(graph, targets)
}

/// Incoming dependency sources of `node`: the `from` end of every edge whose
/// `to` is `node`, deduped and sorted by `display_name`. Empty if `node` has
/// no incoming edges.
pub fn dependent_sources(graph: &ProjectGraph, node: &NodeId) -> Vec<NodeId> {
    let sources = graph
        .edges
        .iter()
        .filter(|edge| &edge.to == node)
        .map(|edge| edge.from.clone());
    dedup_sorted_by_name(graph, sources)
}

/// Dedup `ids` and sort the result by each node's `display_name`, dropping
/// any id that isn't present in `graph`.
fn dedup_sorted_by_name(graph: &ProjectGraph, ids: impl Iterator<Item = NodeId>) -> Vec<NodeId> {
    let mut seen: Vec<NodeId> = Vec::new();
    for id in ids {
        if !seen.contains(&id) {
            seen.push(id);
        }
    }
    let mut named: Vec<(&str, NodeId)> = seen
        .into_iter()
        .filter_map(|id| Some((graph.node(&id)?.display_name.as_str(), id)))
        .collect();
    named.sort_by(|a, b| a.0.cmp(b.0));
    named.into_iter().map(|(_, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepEdge, DepKind, GitStatus, ModuleNode, ProjectGraph};
    use std::collections::HashMap;

    /// Two-root, two-level fixture:
    /// ```text
    /// app (root)          zzz (root, leaf)
    ///  ├─ a (leaf)
    ///  └─ b
    ///      └─ x (leaf)
    /// ```
    /// `app`'s children sort as `[a, b]`; roots sort as `[app, zzz]`.
    fn fixture() -> ProjectGraph {
        let app = NodeId::from("app");
        let zzz = NodeId::from("zzz");
        let a = NodeId::from("a");
        let b = NodeId::from("b");
        let x = NodeId::from("x");

        let leaf = |id: &NodeId, name: &str, parent: Option<NodeId>| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent,
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![],
        };

        let mut nodes = HashMap::new();
        nodes.insert(
            app.clone(),
            ModuleNode {
                id: app.clone(),
                display_name: "app".to_string(),
                parent: None,
                children: vec![b.clone(), a.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(zzz.clone(), leaf(&zzz, "zzz", None));
        nodes.insert(a.clone(), leaf(&a, "a", Some(app.clone())));
        nodes.insert(
            b.clone(),
            ModuleNode {
                id: b.clone(),
                display_name: "b".to_string(),
                parent: Some(app.clone()),
                children: vec![x.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(x.clone(), leaf(&x, "x", Some(b.clone())));

        ProjectGraph {
            nodes,
            roots: vec![app, zzz],
            edges: vec![],
        }
    }

    #[test]
    fn down_moves_to_next_sibling_no_wrap() {
        let g = fixture();
        assert_eq!(
            move_focus(&g, &NodeId::from("app"), Direction::Down),
            NodeId::from("zzz")
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("zzz"), Direction::Down),
            NodeId::from("zzz"),
            "no-op at last root sibling"
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("a"), Direction::Down),
            NodeId::from("b")
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("b"), Direction::Down),
            NodeId::from("b"),
            "no-op at last child sibling"
        );
    }

    #[test]
    fn up_moves_to_prev_sibling_no_wrap() {
        let g = fixture();
        assert_eq!(
            move_focus(&g, &NodeId::from("zzz"), Direction::Up),
            NodeId::from("app")
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("app"), Direction::Up),
            NodeId::from("app"),
            "no-op at first root sibling"
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("b"), Direction::Up),
            NodeId::from("a")
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("a"), Direction::Up),
            NodeId::from("a"),
            "no-op at first child sibling"
        );
    }

    #[test]
    fn right_moves_to_first_child_no_op_on_leaf() {
        let g = fixture();
        assert_eq!(
            move_focus(&g, &NodeId::from("app"), Direction::Right),
            NodeId::from("a")
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("b"), Direction::Right),
            NodeId::from("x")
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("a"), Direction::Right),
            NodeId::from("a"),
            "no-op on leaf"
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("zzz"), Direction::Right),
            NodeId::from("zzz"),
            "no-op on childless root"
        );
    }

    #[test]
    fn left_moves_to_parent_no_op_on_root() {
        let g = fixture();
        assert_eq!(
            move_focus(&g, &NodeId::from("x"), Direction::Left),
            NodeId::from("b")
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("b"), Direction::Left),
            NodeId::from("app")
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("app"), Direction::Left),
            NodeId::from("app"),
            "no-op on root"
        );
        assert_eq!(
            move_focus(&g, &NodeId::from("zzz"), Direction::Left),
            NodeId::from("zzz"),
            "no-op on root"
        );
    }

    /// Standalone edge fixture (disjoint from `fixture()`, no hierarchy
    /// needed): `n1` depends on `alpha` and `beta` (with a duplicate `n1 ->
    /// beta` edge of a different kind, to exercise dedup), and is depended on
    /// by `delta` and `gamma`. `alpha`/`beta` have no outgoing edges of their
    /// own (covering `dep_targets`' 0-edge case); `delta`/`gamma` have no
    /// incoming edges of their own (covering `dependent_sources`' 0-edge
    /// case).
    fn edge_fixture() -> ProjectGraph {
        let n1 = NodeId::from("n1");
        let alpha = NodeId::from("alpha"); // display_name "alpha", id != name to test name-sort
        let beta = NodeId::from("beta");
        let delta = NodeId::from("delta");
        let gamma = NodeId::from("gamma");

        let leaf = |id: &NodeId, name: &str| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![],
        };

        let mut nodes = HashMap::new();
        for (id, name) in [
            (&n1, "n1"),
            (&alpha, "alpha"),
            (&beta, "beta"),
            (&delta, "delta"),
            (&gamma, "gamma"),
        ] {
            nodes.insert(id.clone(), leaf(id, name));
        }

        ProjectGraph {
            roots: nodes.keys().cloned().collect(),
            nodes,
            edges: vec![
                DepEdge {
                    from: n1.clone(),
                    to: beta.clone(),
                    kind: DepKind::Use,
                },
                DepEdge {
                    from: n1.clone(),
                    to: alpha.clone(),
                    kind: DepKind::Import,
                },
                DepEdge {
                    from: n1.clone(),
                    to: beta.clone(),
                    kind: DepKind::Alias,
                },
                DepEdge {
                    from: delta.clone(),
                    to: n1.clone(),
                    kind: DepKind::Require,
                },
                DepEdge {
                    from: gamma.clone(),
                    to: n1.clone(),
                    kind: DepKind::XrefCall,
                },
            ],
        }
    }

    #[test]
    fn dep_targets_dedups_and_sorts_by_name() {
        let g = edge_fixture();
        assert_eq!(
            dep_targets(&g, &NodeId::from("n1")),
            vec![NodeId::from("alpha"), NodeId::from("beta")]
        );
    }

    #[test]
    fn dep_targets_empty_for_node_with_no_outgoing_edges() {
        let g = edge_fixture();
        assert_eq!(dep_targets(&g, &NodeId::from("beta")), Vec::new());
    }

    #[test]
    fn dependent_sources_sorted_by_name() {
        let g = edge_fixture();
        assert_eq!(
            dependent_sources(&g, &NodeId::from("n1")),
            vec![NodeId::from("delta"), NodeId::from("gamma")]
        );
    }

    #[test]
    fn dependent_sources_empty_for_node_with_no_incoming_edges() {
        let g = edge_fixture();
        assert_eq!(dependent_sources(&g, &NodeId::from("delta")), Vec::new());
    }
}
