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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{GitStatus, ModuleNode, ProjectGraph};
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
}
