//! Dependency-depth layering: assigns every *drawn* node (a real module/file
//! node, as opposed to a synthetic namespace container -- see
//! [`ModuleNode::files`]) a layer number, where layer 0 holds the nodes
//! nothing depends on and later layers sink deeper the more other drawn
//! nodes ultimately depend on them. This replaces the old nested-box
//! namespace containment as vdiff's primary visual organization -- see
//! [`crate::graph::layout`], which turns the result into screen geometry,
//! and [`crate::core::focus`], which turns it into h/j/k/l navigation.
//!
//! An edge `A -> B` means "A depends on B", so B should sit visually below
//! A: `layer(B) = 1 + max(layer(A))` over every incoming edge, i.e. the
//! *longest* path from the layer-0 frontier (a shortest-path/BFS layering
//! would let a node with one long and one short incoming path sit above a
//! dependency reachable by the long path, which reads wrong). Elixir
//! dependency graphs do have real cycles (mutual `alias`/xref references
//! between modules), so before layering we run a DFS over the drawn-node
//! subgraph and discard any back edge (an edge into a node still on the
//! current DFS stack) -- back edges are exactly the ones that would make
//! longest-path layering loop forever, and dropping them leaves a DAG
//! without needing to decide which module "really" belongs above the other.

use std::collections::{HashMap, HashSet};

use crate::graph::model::{NodeId, ProjectGraph};

/// Assign every drawn node (see the module docs) a layer, returned as one
/// `Vec<NodeId>` per layer, outermost-first (layer 0 first). Nodes with no
/// edges at all are appended as a trailing layer after every connected
/// layer, so they don't pollute layer 0. Within a layer, nodes are ordered
/// by `(top-level root id, display_name)` so namespace-mates sit adjacent.
pub fn assign_layers(graph: &ProjectGraph) -> Vec<Vec<NodeId>> {
    let drawn = drawn_node_ids(graph);
    let edges = drawn_edges(graph, &drawn);

    let connected: HashSet<NodeId> = edges
        .iter()
        .flat_map(|(from, to)| [from.clone(), to.clone()])
        .collect();
    let isolated: Vec<NodeId> = drawn
        .iter()
        .filter(|id| !connected.contains(*id))
        .cloned()
        .collect();

    let acyclic_edges = drop_back_edges(&connected, &edges, graph);
    let layer_of = longest_path_layers(&connected, &acyclic_edges);

    let mut by_layer: HashMap<usize, Vec<NodeId>> = HashMap::new();
    for (id, layer) in &layer_of {
        by_layer.entry(*layer).or_default().push(id.clone());
    }

    let max_layer = by_layer.keys().copied().max();
    let mut layers: Vec<Vec<NodeId>> = match max_layer {
        Some(max) => (0..=max)
            .map(|l| by_layer.remove(&l).unwrap_or_default())
            .collect(),
        None => Vec::new(),
    };
    for layer in &mut layers {
        sort_by_root_then_name(graph, layer);
    }

    if !isolated.is_empty() {
        let mut trailing = isolated;
        sort_by_root_then_name(graph, &mut trailing);
        layers.push(trailing);
    }

    layers
}

/// Nodes with at least one backing file -- the ones actually painted.
/// Synthetic namespace containers (`files.is_empty()`) are excluded: they
/// stay in the model for id/ancestry lookups but never get a layer.
fn drawn_node_ids(graph: &ProjectGraph) -> Vec<NodeId> {
    graph
        .nodes
        .values()
        .filter(|node| !node.files.is_empty())
        .map(|node| node.id.clone())
        .collect()
}

/// Deduped `(from, to)` pairs for every edge whose endpoints are both drawn
/// nodes. Edges touching a synthetic node shouldn't exist (dependency
/// resolution targets real modules) but are skipped defensively. Multiple
/// [`DepEdge`]s between the same pair (different [`DepKind`]s) collapse to
/// one graph edge -- layering only cares about reachability, not kind.
///
/// [`DepEdge`]: crate::graph::model::DepEdge
/// [`DepKind`]: crate::graph::model::DepKind
fn drawn_edges(graph: &ProjectGraph, drawn: &[NodeId]) -> Vec<(NodeId, NodeId)> {
    let drawn_set: HashSet<&NodeId> = drawn.iter().collect();
    let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
    let mut ordered = Vec::new();
    for edge in &graph.edges {
        if !drawn_set.contains(&edge.from) || !drawn_set.contains(&edge.to) {
            continue;
        }
        let pair = (edge.from.clone(), edge.to.clone());
        if seen.insert(pair.clone()) {
            ordered.push(pair);
        }
    }
    ordered
}

/// DFS over the connected drawn-node subgraph, in `(root, display_name)`
/// order for determinism, discarding any edge into a node still on the
/// current DFS stack (a back edge -- the source of cycles). What's left is
/// a DAG covering the same node set.
fn drop_back_edges(
    connected: &HashSet<NodeId>,
    edges: &[(NodeId, NodeId)],
    graph: &ProjectGraph,
) -> Vec<(NodeId, NodeId)> {
    let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (from, to) in edges {
        adjacency.entry(from.clone()).or_default().push(to.clone());
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let mut color: HashMap<NodeId, Color> = connected
        .iter()
        .map(|id| (id.clone(), Color::White))
        .collect();
    let mut kept: Vec<(NodeId, NodeId)> = Vec::new();

    let mut starts: Vec<NodeId> = connected.iter().cloned().collect();
    sort_by_root_then_name(graph, &mut starts);

    for start in starts {
        if color.get(&start) != Some(&Color::White) {
            continue;
        }
        // Explicit stack: (node, index into its already-sorted neighbor
        // list). Iterative to avoid recursion depth limits on large graphs.
        let mut neighbors_of: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        let mut stack: Vec<(NodeId, usize)> = vec![(start.clone(), 0)];
        color.insert(start.clone(), Color::Gray);

        while let Some((node, idx)) = stack.pop() {
            let neighbors = neighbors_of.entry(node.clone()).or_insert_with(|| {
                let mut ns = adjacency.get(&node).cloned().unwrap_or_default();
                sort_by_root_then_name(graph, &mut ns);
                ns
            });
            if idx >= neighbors.len() {
                color.insert(node.clone(), Color::Black);
                continue;
            }
            let next = neighbors[idx].clone();
            stack.push((node.clone(), idx + 1));
            match color.get(&next).copied().unwrap_or(Color::Black) {
                Color::White => {
                    kept.push((node.clone(), next.clone()));
                    color.insert(next.clone(), Color::Gray);
                    stack.push((next, 0));
                }
                Color::Gray => {
                    // Back edge: drop it (don't push to `kept`).
                }
                Color::Black => {
                    kept.push((node, next));
                }
            }
        }
    }

    kept
}

/// Longest-path layer assignment over a DAG: layer 0 for nodes with no
/// incoming edge, `layer(v) = 1 + max(layer(u))` over incoming edges
/// `u -> v`, computed via Kahn's algorithm so every predecessor is
/// processed before its successors.
fn longest_path_layers(
    connected: &HashSet<NodeId>,
    edges: &[(NodeId, NodeId)],
) -> HashMap<NodeId, usize> {
    let mut out_adj: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut in_degree: HashMap<NodeId, usize> =
        connected.iter().map(|id| (id.clone(), 0)).collect();
    for (from, to) in edges {
        out_adj.entry(from.clone()).or_default().push(to.clone());
        *in_degree.entry(to.clone()).or_insert(0) += 1;
    }

    let mut layer: HashMap<NodeId, usize> = connected.iter().map(|id| (id.clone(), 0)).collect();
    let mut queue: Vec<NodeId> = connected
        .iter()
        .filter(|id| in_degree.get(*id).copied().unwrap_or(0) == 0)
        .cloned()
        .collect();
    let mut remaining_in_degree = in_degree;

    let mut head = 0;
    while head < queue.len() {
        let node = queue[head].clone();
        head += 1;
        let node_layer = layer[&node];
        if let Some(successors) = out_adj.get(&node) {
            for succ in successors {
                let candidate = node_layer + 1;
                let entry = layer.entry(succ.clone()).or_insert(0);
                if candidate > *entry {
                    *entry = candidate;
                }
                let deg = remaining_in_degree.entry(succ.clone()).or_insert(0);
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    queue.push(succ.clone());
                }
            }
        }
    }

    layer
}

/// Sort `ids` by `(top-level root id, display_name)`, dropping any id not
/// present in `graph`.
fn sort_by_root_then_name(graph: &ProjectGraph, ids: &mut Vec<NodeId>) {
    ids.retain(|id| graph.node(id).is_some());
    ids.sort_by(|a, b| {
        let root_a = top_level_root(graph, a);
        let root_b = top_level_root(graph, b);
        let name_a = graph.node(a).map(|n| n.display_name.as_str()).unwrap_or("");
        let name_b = graph.node(b).map(|n| n.display_name.as_str()).unwrap_or("");
        (root_a, name_a).cmp(&(root_b, name_b))
    });
}

/// Walk `id`'s parent chain up to its top-level ancestor (the root with no
/// parent), returning that root's id. May itself be a synthetic namespace
/// node -- that's fine, it's only used as a grouping/coloring key, never
/// drawn or navigated to directly. Returns `id` itself if it's unknown or
/// already a root.
fn top_level_root(graph: &ProjectGraph, id: &NodeId) -> NodeId {
    let mut current = id.clone();
    loop {
        match graph.node(&current).and_then(|n| n.parent.clone()) {
            Some(parent) => current = parent,
            None => return current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepEdge, DepKind, GitStatus, ModuleNode};
    use std::path::PathBuf;

    /// A drawn leaf node: has a parent (possibly synthetic) and one backing
    /// file, so `drawn_node_ids` picks it up.
    fn leaf(id: &str, name: &str, parent: Option<&str>) -> (NodeId, ModuleNode) {
        let node_id = NodeId::from(id);
        (
            node_id.clone(),
            ModuleNode {
                id: node_id,
                display_name: name.to_string(),
                parent: parent.map(NodeId::from),
                children: vec![],
                status: GitStatus::Unchanged,
                files: vec![FileRefStub::stub(id)],
            },
        )
    }

    /// A synthetic namespace container: no files, so it's excluded from
    /// layering entirely.
    fn synthetic(
        id: &str,
        name: &str,
        parent: Option<&str>,
        children: &[&str],
    ) -> (NodeId, ModuleNode) {
        let node_id = NodeId::from(id);
        (
            node_id.clone(),
            ModuleNode {
                id: node_id,
                display_name: name.to_string(),
                parent: parent.map(NodeId::from),
                children: children.iter().map(|c| NodeId::from(*c)).collect(),
                status: GitStatus::Unchanged,
                files: vec![],
            },
        )
    }

    /// Tiny helper so `leaf` doesn't need to spell out a full `FileRef`.
    struct FileRefStub;
    impl FileRefStub {
        fn stub(id: &str) -> crate::graph::model::FileRef {
            crate::graph::model::FileRef {
                path: PathBuf::from(format!("{id}.ex")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }
        }
    }

    fn edge(from: &str, to: &str) -> DepEdge {
        DepEdge {
            from: NodeId::from(from),
            to: NodeId::from(to),
            kind: DepKind::Alias,
        }
    }

    fn graph_from(
        entries: Vec<(NodeId, ModuleNode)>,
        roots: Vec<&str>,
        edges: Vec<DepEdge>,
    ) -> ProjectGraph {
        ProjectGraph {
            nodes: entries.into_iter().collect(),
            roots: roots.into_iter().map(NodeId::from).collect(),
            edges,
        }
    }

    fn ids(names: &[&str]) -> Vec<NodeId> {
        names.iter().map(|n| NodeId::from(*n)).collect()
    }

    #[test]
    fn linear_chain_gives_three_layers_top_down() {
        let g = graph_from(
            vec![
                leaf("a", "a", None),
                leaf("b", "b", None),
                leaf("c", "c", None),
            ],
            vec!["a", "b", "c"],
            vec![edge("a", "b"), edge("b", "c")],
        );

        let layers = assign_layers(&g);

        assert_eq!(layers, vec![ids(&["a"]), ids(&["b"]), ids(&["c"])]);
    }

    #[test]
    fn diamond_puts_the_join_node_below_both_branches() {
        // a -> b -> d, a -> c -> d: d must sit at layer 2 (longest path),
        // not layer 1 (which a shortest-path/BFS layering would give it via
        // whichever branch is processed first).
        let g = graph_from(
            vec![
                leaf("a", "a", None),
                leaf("b", "b", None),
                leaf("c", "c", None),
                leaf("d", "d", None),
            ],
            vec!["a", "b", "c", "d"],
            vec![
                edge("a", "b"),
                edge("a", "c"),
                edge("b", "d"),
                edge("c", "d"),
            ],
        );

        let layers = assign_layers(&g);

        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], ids(&["a"]));
        assert_eq!(layers[1], ids(&["b", "c"]));
        assert_eq!(layers[2], ids(&["d"]));
    }

    #[test]
    fn cycle_reachable_from_a_source_terminates_with_stable_result() {
        // source -> a -> b -> a (cycle). The DFS starting at `source` visits
        // `a` then `b`; `b -> a` finds `a` gray on the stack, so it's
        // dropped as a back edge, leaving a clean source -> a -> b chain.
        let g = graph_from(
            vec![
                leaf("source", "source", None),
                leaf("a", "a", None),
                leaf("b", "b", None),
            ],
            vec!["source", "a", "b"],
            vec![edge("source", "a"), edge("a", "b"), edge("b", "a")],
        );

        let layers = assign_layers(&g);

        assert_eq!(layers, vec![ids(&["source"]), ids(&["a"]), ids(&["b"])]);
    }

    #[test]
    fn cycle_runs_twice_with_identical_result() {
        // Same fixture as above, called twice -- guards against any hidden
        // nondeterminism (e.g. HashMap/HashSet iteration order) leaking into
        // which edge gets dropped as the back edge.
        let g = graph_from(
            vec![
                leaf("source", "source", None),
                leaf("a", "a", None),
                leaf("b", "b", None),
            ],
            vec!["source", "a", "b"],
            vec![edge("source", "a"), edge("a", "b"), edge("b", "a")],
        );

        assert_eq!(assign_layers(&g), assign_layers(&g));
    }

    #[test]
    fn no_edge_nodes_land_in_a_trailing_layer_after_connected_ones() {
        let g = graph_from(
            vec![
                leaf("a", "a", None),
                leaf("b", "b", None),
                leaf("lonely", "lonely", None),
            ],
            vec!["a", "b", "lonely"],
            vec![edge("a", "b")],
        );

        let layers = assign_layers(&g);

        assert_eq!(layers.len(), 3, "a, b, then a trailing layer for `lonely`");
        assert_eq!(layers[0], ids(&["a"]));
        assert_eq!(layers[1], ids(&["b"]));
        assert_eq!(layers[2], ids(&["lonely"]));
    }

    #[test]
    fn synthetic_namespace_nodes_are_absent_from_every_layer() {
        let g = graph_from(
            vec![
                synthetic("ns", "Ns", None, &["ns::leaf"]),
                leaf("ns::leaf", "leaf", Some("ns")),
            ],
            vec!["ns"],
            vec![],
        );

        let layers = assign_layers(&g);

        let all: Vec<&NodeId> = layers.iter().flatten().collect();
        assert!(
            !all.contains(&&NodeId::from("ns")),
            "synthetic node must not be drawn"
        );
        assert_eq!(all, vec![&NodeId::from("ns::leaf")]);
    }

    #[test]
    fn edges_touching_a_synthetic_node_are_defensively_skipped() {
        // Shouldn't happen in practice (dependency resolution targets real
        // modules), but layering must not panic or misbehave if it does.
        let g = graph_from(
            vec![
                synthetic("ns", "Ns", None, &["ns::leaf"]),
                leaf("ns::leaf", "leaf", Some("ns")),
                leaf("other", "other", None),
            ],
            vec!["ns", "other"],
            vec![edge("ns", "other"), edge("other", "ns")],
        );

        let layers = assign_layers(&g);

        // Both drawn nodes end up disconnected (their only edges touched
        // the synthetic node), so both land in the trailing layer together.
        assert_eq!(layers.len(), 1);
        let mut names: Vec<&str> = layers[0]
            .iter()
            .map(|id| g.node(id).unwrap().display_name.as_str())
            .collect();
        names.sort();
        assert_eq!(names, vec!["leaf", "other"]);
    }

    #[test]
    fn within_a_layer_nodes_are_ordered_by_root_then_name() {
        // Two disjoint roots, each with a leaf at the same layer (no
        // edges): both land in the trailing layer, ordered by root id first
        // (`root_a` < `root_b`), then name within a root.
        let g = graph_from(
            vec![
                synthetic("root_b", "root_b", None, &["root_b::z"]),
                leaf("root_b::z", "z", Some("root_b")),
                synthetic("root_a", "root_a", None, &["root_a::y"]),
                leaf("root_a::y", "y", Some("root_a")),
            ],
            vec!["root_a", "root_b"],
            vec![],
        );

        let layers = assign_layers(&g);

        assert_eq!(layers, vec![ids(&["root_a::y", "root_b::z"])]);
    }
}
