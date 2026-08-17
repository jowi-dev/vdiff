//! h/j/k/l layer navigation over the layered dependency layout: pure,
//! consulting only the layer structure ([`crate::graph::layers::assign_layers`]'s
//! output, threaded in as `layers`) -- never layout geometry. h/l move
//! within a layer's row; j/k move between adjacent layers.

use crate::graph::model::{NodeId, ProjectGraph};

/// A single navigation step, named after its vim keybinding (see
/// [`crate::keymap`], which maps h/j/k/l to these unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `h`: previous node in the current layer's row. No-op at the row's
    /// start.
    Left,
    /// `j`: the roughly-same-position node one layer down (deeper
    /// dependency). No-op on the last layer.
    Down,
    /// `k`: the roughly-same-position node one layer up (shallower
    /// dependency). No-op on the first layer.
    Up,
    /// `l`: next node in the current layer's row. No-op at the row's end.
    Right,
}

/// Move focus from `current` in `dir`, returning the new focused node.
/// `layers` is [`crate::graph::layers::assign_layers`]'s output (one row of
/// node ids per layer, already ordered root-then-name). Returns `current`
/// unchanged if it isn't found in `layers` at all (shouldn't happen --
/// [`crate::core::app::App::layers`] and `focus` are built from the same
/// graph -- but `FocusSet` defensively rejects synthetic/unknown targets
/// too, so this stays a safe fallback) or for the no-op cases documented on
/// [`Direction`]'s variants.
pub fn move_focus(layers: &[Vec<NodeId>], current: &NodeId, dir: Direction) -> NodeId {
    let Some((layer_idx, pos_idx)) = locate(layers, current) else {
        return current.clone();
    };

    match dir {
        Direction::Left => step_within_row(layers, layer_idx, pos_idx, -1),
        Direction::Right => step_within_row(layers, layer_idx, pos_idx, 1),
        Direction::Up => step_to_layer(layers, layer_idx, pos_idx, -1),
        Direction::Down => step_to_layer(layers, layer_idx, pos_idx, 1),
    }
    .unwrap_or_else(|| current.clone())
}

/// Find `id`'s `(layer_idx, pos_idx)` in `layers`.
fn locate(layers: &[Vec<NodeId>], id: &NodeId) -> Option<(usize, usize)> {
    for (layer_idx, row) in layers.iter().enumerate() {
        if let Some(pos_idx) = row.iter().position(|n| n == id) {
            return Some((layer_idx, pos_idx));
        }
    }
    None
}

/// Step `delta` positions within `layers[layer_idx]`, clamped to the row's
/// bounds. `None` if the step would be a no-op (already at that end).
fn step_within_row(
    layers: &[Vec<NodeId>],
    layer_idx: usize,
    pos_idx: usize,
    delta: i32,
) -> Option<NodeId> {
    let row = &layers[layer_idx];
    let new_pos = pos_idx as i32 + delta;
    if new_pos < 0 || new_pos as usize >= row.len() {
        return None;
    }
    row.get(new_pos as usize).cloned()
}

/// Move to `layers[layer_idx + delta]` (`delta` is `1` or `-1`), landing on
/// the node at the same fractional position within that row: `pos_idx *
/// target_len / current_len`, clamped to the target row's last index. A
/// cheap "stay roughly above/below" without consulting layout geometry.
/// `None` if `layer_idx + delta` is out of bounds or the target layer is
/// empty (shouldn't happen -- `assign_layers` never emits an empty layer).
fn step_to_layer(
    layers: &[Vec<NodeId>],
    layer_idx: usize,
    pos_idx: usize,
    delta: i32,
) -> Option<NodeId> {
    let target_idx = layer_idx as i32 + delta;
    if target_idx < 0 || target_idx as usize >= layers.len() {
        return None;
    }
    let target_idx = target_idx as usize;
    let target_row = &layers[target_idx];
    if target_row.is_empty() {
        return None;
    }
    let current_len = layers[layer_idx].len().max(1);
    let target_len = target_row.len();
    let target_pos = (pos_idx * target_len / current_len).min(target_len - 1);
    target_row.get(target_pos).cloned()
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
    use crate::graph::model::{DepEdge, DepKind, GitStatus, ModuleNode};
    use std::collections::HashMap;

    fn id(name: &str) -> NodeId {
        NodeId::from(name)
    }

    fn row(names: &[&str]) -> Vec<NodeId> {
        names.iter().map(|n| id(n)).collect()
    }

    /// Three layers: `[app, zzz]`, `[a, b]`, `[x]` -- enough rows/lengths to
    /// exercise clamping (row-end no-ops) and the fractional j/k landing
    /// (2 -> 1 nodes and 1 -> 2 nodes).
    fn layers_fixture() -> Vec<Vec<NodeId>> {
        vec![row(&["app", "zzz"]), row(&["a", "b"]), row(&["x"])]
    }

    #[test]
    fn right_and_left_move_within_a_row_no_wrap() {
        let layers = layers_fixture();
        assert_eq!(move_focus(&layers, &id("app"), Direction::Right), id("zzz"));
        assert_eq!(
            move_focus(&layers, &id("zzz"), Direction::Right),
            id("zzz"),
            "no-op at row end"
        );
        assert_eq!(move_focus(&layers, &id("zzz"), Direction::Left), id("app"));
        assert_eq!(
            move_focus(&layers, &id("app"), Direction::Left),
            id("app"),
            "no-op at row start"
        );
    }

    #[test]
    fn down_and_up_move_between_layers_at_fractional_position() {
        let layers = layers_fixture();
        // layer 0 pos 0 (of 2) -> layer 1 (of 2): 0*2/2 = 0 -> "a".
        assert_eq!(move_focus(&layers, &id("app"), Direction::Down), id("a"));
        // layer 0 pos 1 (of 2) -> layer 1 (of 2): 1*2/2 = 1 -> "b".
        assert_eq!(move_focus(&layers, &id("zzz"), Direction::Down), id("b"));
        // layer 1 pos 1 (of 2) -> layer 2 (of 1): 1*1/2 = 0 -> "x".
        assert_eq!(move_focus(&layers, &id("b"), Direction::Down), id("x"));
        // layer 2 pos 0 (of 1) -> layer 1 (of 2): 0*2/1 = 0 -> "a".
        assert_eq!(move_focus(&layers, &id("x"), Direction::Up), id("a"));
    }

    #[test]
    fn down_noop_on_last_layer_up_noop_on_first_layer() {
        let layers = layers_fixture();
        assert_eq!(move_focus(&layers, &id("x"), Direction::Down), id("x"));
        assert_eq!(move_focus(&layers, &id("app"), Direction::Up), id("app"));
        assert_eq!(move_focus(&layers, &id("zzz"), Direction::Up), id("zzz"));
    }

    #[test]
    fn unknown_node_returns_itself() {
        let layers = layers_fixture();
        assert_eq!(
            move_focus(&layers, &id("ghost"), Direction::Right),
            id("ghost")
        );
        assert_eq!(
            move_focus(&layers, &id("ghost"), Direction::Down),
            id("ghost")
        );
    }

    /// Standalone edge fixture (no layer hierarchy needed): `n1` depends on
    /// `alpha` and `beta` (with a duplicate `n1 -> beta` edge of a different
    /// kind, to exercise dedup), and is depended on by `delta` and `gamma`.
    /// `alpha`/`beta` have no outgoing edges of their own (covering
    /// `dep_targets`' 0-edge case); `delta`/`gamma` have no incoming edges of
    /// their own (covering `dependent_sources`' 0-edge case).
    fn edge_fixture() -> ProjectGraph {
        let n1 = NodeId::from("n1");
        let alpha = NodeId::from("alpha");
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
