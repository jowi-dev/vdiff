//! h/j/k/l layer navigation over the layered dependency layout: pure,
//! consulting the layer structure ([`crate::graph::layers::assign_layers`]'s
//! output, threaded in as `layers`) for h/l, and the row structure
//! ([`crate::graph::layout::rows_with_x_centers`]'s output, threaded in as
//! `rows`) for j/k. h/l move within a layer's row, ignoring how it wraps
//! onto screen; j/k move between adjacent *visual* rows (a wrapped layer's
//! sub-rows count as rows in their own right), landing on the x-nearest
//! node -- see [`move_focus`]'s doc for why layer structure alone isn't
//! enough for that.

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
/// node ids per layer, already ordered root-then-name) -- `h`/`l` walk it
/// unchanged, in a layer's node order regardless of how that layer wraps
/// onto multiple visual rows on screen (a linear "next/previous in this
/// layer" model, where wrapping across sub-rows is fine). `rows` is
/// [`crate::graph::layout::rows_with_x_centers`]'s output (one row per
/// *visual* row -- a wrapped layer contributes multiple entries -- each
/// node paired with its rect's x-center): `j`/`k` land on the node in the
/// adjacent visual row whose x-center is nearest `current`'s, which is what
/// makes `j` step to the node actually below `current` on screen even when
/// that's a wrapped sub-row of the same layer, not a jump to the next whole
/// layer (the old fractional-layer-index behavior this replaced could skip
/// right over it). Returns `current` unchanged if it isn't found in the
/// relevant structure at all (shouldn't happen --
/// [`crate::core::app::App::layers`]/`rows` and `focus` are built from the
/// same graph -- but `FocusSet` defensively rejects synthetic/unknown
/// targets too, so this stays a safe fallback) or for the no-op cases
/// documented on [`Direction`]'s variants.
pub fn move_focus(
    layers: &[Vec<NodeId>],
    rows: &[Vec<(NodeId, f32)>],
    current: &NodeId,
    dir: Direction,
) -> NodeId {
    match dir {
        Direction::Left => locate(layers, current)
            .and_then(|(layer_idx, pos_idx)| step_within_row(layers, layer_idx, pos_idx, -1)),
        Direction::Right => locate(layers, current)
            .and_then(|(layer_idx, pos_idx)| step_within_row(layers, layer_idx, pos_idx, 1)),
        Direction::Up => step_to_visual_row(rows, current, -1),
        Direction::Down => step_to_visual_row(rows, current, 1),
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

/// Find `id`'s `(row_idx, x_center)` in `rows`.
fn locate_in_rows(rows: &[Vec<(NodeId, f32)>], id: &NodeId) -> Option<(usize, f32)> {
    for (row_idx, row) in rows.iter().enumerate() {
        if let Some((_, x_center)) = row.iter().find(|(node_id, _)| node_id == id) {
            return Some((row_idx, *x_center));
        }
    }
    None
}

/// Move to `rows[row_idx + delta]` (`delta` is `1` or `-1`), landing on the
/// node whose x-center is nearest `current`'s -- ties broken toward the
/// earlier (leftmost) candidate for determinism. `None` if `current` isn't
/// in `rows`, `row_idx + delta` is out of bounds, or the target row is
/// empty (shouldn't happen -- [`crate::graph::layout::layout`] never emits
/// an empty row).
fn step_to_visual_row(rows: &[Vec<(NodeId, f32)>], current: &NodeId, delta: i32) -> Option<NodeId> {
    let (row_idx, current_x) = locate_in_rows(rows, current)?;
    let target_idx = row_idx as i32 + delta;
    if target_idx < 0 || target_idx as usize >= rows.len() {
        return None;
    }
    let target_row = &rows[target_idx as usize];
    target_row
        .iter()
        .min_by(|(_, a), (_, b)| {
            (a - current_x)
                .abs()
                .partial_cmp(&(b - current_x).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(id, _)| id.clone())
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
    /// exercise clamping (row-end no-ops) for `h`/`l`, which walk `layers`
    /// unchanged regardless of visual wrapping.
    fn layers_fixture() -> Vec<Vec<NodeId>> {
        vec![row(&["app", "zzz"]), row(&["a", "b"]), row(&["x"])]
    }

    fn xy(name: &str, x: f32) -> (NodeId, f32) {
        (id(name), x)
    }

    /// Three *visual* rows, deliberately not a 1:1 mapping with any layer
    /// list: layer 0 wraps onto two sub-rows (`p1`/`p2` then `p3`/`p4`,
    /// matching x-columns 10/100 in both), then a single-row layer 1
    /// (`q1` at x=50) -- exactly the shape `j`/`k` must navigate correctly
    /// (a wrapped sub-row within one layer, immediately followed by the
    /// next layer's row).
    fn rows_fixture() -> Vec<Vec<(NodeId, f32)>> {
        vec![
            vec![xy("p1", 10.0), xy("p2", 100.0)],
            vec![xy("p3", 10.0), xy("p4", 100.0)],
            vec![xy("q1", 50.0)],
        ]
    }

    #[test]
    fn right_and_left_move_within_a_row_no_wrap() {
        let layers = layers_fixture();
        assert_eq!(
            move_focus(&layers, &[], &id("app"), Direction::Right),
            id("zzz")
        );
        assert_eq!(
            move_focus(&layers, &[], &id("zzz"), Direction::Right),
            id("zzz"),
            "no-op at row end"
        );
        assert_eq!(
            move_focus(&layers, &[], &id("zzz"), Direction::Left),
            id("app")
        );
        assert_eq!(
            move_focus(&layers, &[], &id("app"), Direction::Left),
            id("app"),
            "no-op at row start"
        );
    }

    #[test]
    fn down_lands_on_the_node_directly_below_in_a_wrapped_sub_row() {
        // `j` from `p1` (x=10) must land on `p3` (same column, next visual
        // row) -- not skip past it to layer 1's `q1` the way the old
        // fractional-layer-index `j` would have (it only ever consulted
        // whole layers, never wrapped sub-rows).
        let rows = rows_fixture();
        assert_eq!(move_focus(&[], &rows, &id("p1"), Direction::Down), id("p3"));
        assert_eq!(move_focus(&[], &rows, &id("p2"), Direction::Down), id("p4"));
    }

    #[test]
    fn down_crosses_from_the_last_wrapped_sub_row_into_the_next_layer() {
        // `p3`/`p4` are the last visual row of layer 0's wrapped band --
        // `j` from there must reach layer 1's row (`q1`), landing on
        // whichever is x-nearest.
        let rows = rows_fixture();
        assert_eq!(move_focus(&[], &rows, &id("p3"), Direction::Down), id("q1"));
        assert_eq!(move_focus(&[], &rows, &id("p4"), Direction::Down), id("q1"));
    }

    #[test]
    fn up_lands_on_the_x_nearest_node_in_the_row_above() {
        // From `q1` (x=50), the row above is `p3`/`p4` (x=10/x=100): `p3`
        // is closer (distance 40 vs 50), so `k` lands there.
        let rows = rows_fixture();
        assert_eq!(move_focus(&[], &rows, &id("q1"), Direction::Up), id("p3"));
    }

    #[test]
    fn down_noop_on_last_row_up_noop_on_first_row() {
        let rows = rows_fixture();
        assert_eq!(move_focus(&[], &rows, &id("q1"), Direction::Down), id("q1"));
        assert_eq!(move_focus(&[], &rows, &id("p1"), Direction::Up), id("p1"));
        assert_eq!(move_focus(&[], &rows, &id("p2"), Direction::Up), id("p2"));
    }

    #[test]
    fn unknown_node_returns_itself() {
        let layers = layers_fixture();
        let rows = rows_fixture();
        assert_eq!(
            move_focus(&layers, &rows, &id("ghost"), Direction::Right),
            id("ghost")
        );
        assert_eq!(
            move_focus(&layers, &rows, &id("ghost"), Direction::Down),
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
