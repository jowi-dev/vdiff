//! Fold-by-namespace: the `--tui` graph screen's "zoom" mechanic (issue
//! #16 phase 2). Pure derivation of the *visible* row list from
//! [`crate::core::app::App::layers`] plus a set of collapsed namespace ids,
//! and the edge set the rail gutter ([`crate::graph::rails`]) should render
//! once collapsed rows have absorbed their descendants' edges.
//!
//! # Where fold state lives, and why
//!
//! Fold state ([`crate::core::app::App::fold_collapsed`]) sits on
//! `core::App` rather than as TUI-local state (unlike, say,
//! `crate::tui::TuiState::notice`, which is deliberately *not* on `App` --
//! see that field's own doc for why). The difference: `notice` is pure
//! display glue with zero effect on navigation, whereas fold state changes
//! what "the next/previous visible row" even means -- `j`/`k` in the rail
//! view must skip over a collapsed namespace's absorbed descendants
//! entirely, and `gd`/`gr`'s picker candidates, `Msg::FocusSet`'s
//! drawn-node check, and `Msg::ToggleTests`'s re-seat logic all reason
//! about "what's focusable right now" in ways a fold toggle can change out
//! from under them. That's core navigation semantics, not rendering, so it
//! belongs in `core` alongside `App::show_tests` (which affects the exact
//! same "what's drawn" question for the same reason).
//!
//! This module itself, though, stays TUI-only in *practice* even though it
//! lives in `core`: nothing here is reachable unless something calls
//! [`visible_rows`]/[`collapse_edges`], and only `crate::tui` does. The GUI
//! never collapses anything, so `App::fold_collapsed` is simply always
//! empty on that path, and every function here is then the identity
//! transform (every row visible, every edge un-collapsed) -- there's no
//! GUI-side behavior change to account for.
//!
//! # Visible-row derivation
//!
//! [`visible_rows`] walks `layers` in layer order (top to bottom, matching
//! the graph's dependency depth -- see [`crate::graph::layers`]) and, for
//! each drawn node, checks whether any ancestor in its parent chain is
//! collapsed. If so, every node under that ancestor collapses into a single
//! [`RailRow::Collapsed`] row, emitted once, at the position of the first
//! (shallowest-layer) member encountered -- descendants scattered across
//! multiple layers (a namespace's modules don't all sit at the same
//! dependency depth) still produce exactly one row, not one per layer they
//! touch. Nested collapse (a collapsed namespace itself sitting under
//! another collapsed ancestor) resolves to the *outermost* collapsed
//! ancestor, so collapsing a grandparent after already collapsing a parent
//! correctly absorbs the parent's row into the grandparent's.

use std::collections::HashSet;

use crate::graph::model::{DepEdge, GitStatus, NodeId, ProjectGraph};

/// `j`/`k` in the rail view: move to the next/previous row in
/// [`visible_rows`]'s flattened order. A dedicated enum rather than reusing
/// [`crate::core::focus::Direction`] -- that type's `Left`/`Right` variants
/// have no meaning here (see `crate::tui::mod`'s doc on why `h`/`l` are
/// handled as separate collapse/expand messages instead of directional
/// movement at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailDirection {
    Up,
    Down,
}

/// One row of the rail view's flattened, fold-aware display list: either a
/// single drawn node, or a collapsed namespace absorbing every node under
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailRow {
    Node(NodeId),
    Collapsed {
        /// The namespace node's id -- also what `App::focus` is set to
        /// while this row is focused (see `crate::core::app`'s
        /// `collapse_focused_namespace`/`expand_focused_namespace`).
        namespace: NodeId,
        /// How many drawn (file-backed) descendants this namespace absorbed.
        module_count: usize,
        /// How many of those descendants have a non-[`GitStatus::Unchanged`]
        /// status.
        changed_count: usize,
    },
}

impl RailRow {
    /// This row's identifying [`NodeId`] -- the node itself for
    /// [`RailRow::Node`], the namespace id for [`RailRow::Collapsed`]. What
    /// `App::focus` should be set to when this row is focused.
    pub fn id(&self) -> &NodeId {
        match self {
            RailRow::Node(id) => id,
            RailRow::Collapsed { namespace, .. } => namespace,
        }
    }
}

/// Derive the visible row list from `layers` (see [`crate::graph::layers`])
/// given `collapsed` (the current [`crate::core::app::App::fold_collapsed`]
/// set). Default/empty `collapsed` yields one [`RailRow::Node`] per entry in
/// `layers`, in the same layer-by-layer order -- fully expanded, matching
/// this feature's documented default.
pub fn visible_rows(
    graph: &ProjectGraph,
    layers: &[Vec<NodeId>],
    collapsed: &HashSet<NodeId>,
) -> Vec<RailRow> {
    visible_rows_with_layers(graph, layers, collapsed)
        .into_iter()
        .map(|(row, _layer)| row)
        .collect()
}

/// Like [`visible_rows`], but each row is paired with the original
/// (pre-fold) layer index it was first encountered at while walking
/// `layers` -- what `crate::tui::render`'s band-separator hint needs to
/// know where one layer's rows end and the next begin, even once folding
/// has interleaved/dropped some of them. A collapsed row reports the layer
/// of its *shallowest* absorbed member (the first one `visible_rows`
/// happens to walk into, per the module doc's derivation order).
pub fn visible_rows_with_layers(
    graph: &ProjectGraph,
    layers: &[Vec<NodeId>],
    collapsed: &HashSet<NodeId>,
) -> Vec<(RailRow, usize)> {
    let mut rows = Vec::new();
    let mut emitted: HashSet<NodeId> = HashSet::new();
    for (layer_idx, layer) in layers.iter().enumerate() {
        for id in layer {
            match collapsed_namespace_of(graph, id, collapsed) {
                Some(namespace) => {
                    if emitted.insert(namespace.clone()) {
                        let (module_count, changed_count) = namespace_stats(graph, &namespace);
                        rows.push((
                            RailRow::Collapsed {
                                namespace,
                                module_count,
                                changed_count,
                            },
                            layer_idx,
                        ));
                    }
                }
                None => rows.push((RailRow::Node(id.clone()), layer_idx)),
            }
        }
    }
    rows
}

/// The row id `id` actually renders as: `id` itself if no ancestor is
/// collapsed, or [`collapse_root`]'s result otherwise. Used to translate raw
/// [`DepEdge`]s onto the visible row set in [`collapse_edges`], and by
/// `crate::core::app`'s focus-setting handlers (`gd`/`gr`, the edge picker,
/// `Msg::FocusSet`, `Msg::ToggleTests`'s re-seat) to keep `App::focus`
/// always pointing at something actually present in [`visible_rows`] --
/// see that call site's own doc for why this matters (a focus id with no
/// corresponding visible row soft-locks `j`/`k`/scroll/highlight in the
/// rail view). Identity (`id` unchanged) whenever `collapsed` is empty, so
/// this has no effect on the GUI, which never populates
/// `App::fold_collapsed` at all.
pub fn effective_row_id(graph: &ProjectGraph, id: &NodeId, collapsed: &HashSet<NodeId>) -> NodeId {
    collapsed_namespace_of(graph, id, collapsed).unwrap_or_else(|| id.clone())
}

/// The namespace row `id` belongs to once folding is applied, including
/// `id` itself: `Some(id.clone())` when `id` is directly in `collapsed`,
/// otherwise [`collapse_root`]'s nearest-collapsed-ancestor result. `None`
/// when neither `id` nor any ancestor is collapsed (it renders as its own
/// [`RailRow::Node`]).
///
/// The "`id` itself" case matters because a namespace can be a *drawn* node
/// in its own right -- see `crate::graph::builder`'s "real defmodule takes
/// precedence over synthetic namespace" precedent (e.g. a real
/// `defmodule AppWeb do end` that also has `AppWeb.Foo`/`AppWeb.Bar`
/// submodules) -- so its own entry sits in `layers` right alongside its
/// descendants. [`collapse_root`] only walks *ancestors*, so without this
/// check the namespace's own layer entry would fall through to
/// [`visible_rows_with_layers`]'s `None` arm and render a second,
/// duplicate row for the same [`NodeId`] (one [`RailRow::Node`] for the
/// namespace's own entry, plus the [`RailRow::Collapsed`] its descendants
/// already produce) -- exactly the real-use "same row twice" bug this
/// function exists to close. [`first_visible_descendant`] already applies
/// this same "check `id` itself first" rule for the same reason.
fn collapsed_namespace_of(
    graph: &ProjectGraph,
    id: &NodeId,
    collapsed: &HashSet<NodeId>,
) -> Option<NodeId> {
    if collapsed.contains(id) {
        return Some(id.clone());
    }
    collapse_root(graph, id, collapsed)
}

/// Walk `id`'s parent chain looking for the nearest ancestor present in
/// `collapsed`. `None` if `id` has no collapsed ancestor at all (it renders
/// as its own [`RailRow::Node`]).
fn collapse_root(graph: &ProjectGraph, id: &NodeId, collapsed: &HashSet<NodeId>) -> Option<NodeId> {
    let mut current = graph.node(id)?.parent.clone();
    while let Some(ancestor) = current {
        if collapsed.contains(&ancestor) {
            return Some(ancestor);
        }
        current = graph.node(&ancestor)?.parent.clone();
    }
    None
}

/// `(module_count, changed_count)` for every drawn (file-backed) descendant
/// of `namespace`, recursing through [`crate::graph::model::ModuleNode::children`]
/// regardless of whether an intermediate node itself happens to carry files
/// (shouldn't occur for a real namespace container, but this stays correct
/// either way rather than assuming the shape).
fn namespace_stats(graph: &ProjectGraph, namespace: &NodeId) -> (usize, usize) {
    let mut modules = 0;
    let mut changed = 0;
    collect_stats(graph, namespace, &mut modules, &mut changed);
    (modules, changed)
}

fn collect_stats(graph: &ProjectGraph, id: &NodeId, modules: &mut usize, changed: &mut usize) {
    let Some(node) = graph.node(id) else {
        return;
    };
    if !node.files.is_empty() {
        *modules += 1;
        if node.status != GitStatus::Unchanged {
            *changed += 1;
        }
    }
    for child in &node.children {
        collect_stats(graph, child, modules, changed);
    }
}

/// The bare label [`disambiguated_labels`] compares for collisions: a
/// [`RailRow::Node`]'s own `display_name`, or a [`RailRow::Collapsed`]
/// namespace's `display_name` (the same text `crate::tui::render`'s
/// `node_line`/`collapsed_row_spans` would otherwise render verbatim).
fn raw_label(graph: &ProjectGraph, row: &RailRow) -> String {
    graph
        .node(row.id())
        .map(|n| n.display_name.clone())
        .unwrap_or_else(|| row.id().to_string())
}

/// `id`'s ancestor chain as display names, nearest parent first, root
/// last. Stops (rather than panicking) at the first ancestor id missing
/// from `graph` -- shouldn't happen, but [`disambiguated_labels`] only
/// needs however much of the chain actually resolves.
fn ancestor_names(graph: &ProjectGraph, id: &NodeId) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = graph.node(id).and_then(|n| n.parent.clone());
    while let Some(ancestor) = current {
        let Some(node) = graph.node(&ancestor) else {
            break;
        };
        names.push(node.display_name.clone());
        current = node.parent.clone();
    }
    names
}

/// The separator a qualified label joins its ancestry with: `/` for a
/// file-backed id (`crate::graph::builder`'s `file:` namespace prefix,
/// used for anything outside a recognized module language -- directories
/// read naturally path-style), `.` otherwise (Elixir's/Rust's dotted
/// module-path convention). Read straight off the id's own text rather
/// than importing `crate::graph::builder`'s private `Sep` enum, since this
/// only needs the one bit builder.rs's id convention already encodes.
fn qualifier_separator(id: &NodeId) -> &'static str {
    if id.to_string().starts_with("file:") {
        "/"
    } else {
        "."
    }
}

/// A display label per row in `rows`, qualified with just enough trailing
/// ancestry to tell apart any two *currently visible* rows that would
/// otherwise render the identical bare name -- e.g. two distinct `docs`
/// directories (`file:docs` and `file:priv/static/docs`), or two distinct
/// `Auction` namespaces under different parents. [`NodeId`]s are unique, so
/// this is strictly about distinct ids whose [`raw_label`] happens to
/// collide (contrast [`visible_rows_with_layers`]'s `emitted` dedup, which
/// is about the same id showing up twice); a row whose bare name is unique
/// among `rows` is returned unqualified. Only ids actually present in
/// `rows` are considered -- a name colliding with something currently
/// folded away out of view isn't a collision the user can see, so it isn't
/// qualified.
///
/// Disambiguation is per colliding group: each group's ancestry depth
/// grows by one (nearest ancestor first) until every member's qualified
/// label is distinct from the rest of its own group, so a row only carries
/// as much extra path as its *own* collision actually needs. Shared by
/// `crate::tui::render`'s rail and canvas screens (both call this over the
/// same [`visible_rows_with_layers`] output) so the two views never
/// disagree about which rows are ambiguous.
pub fn disambiguated_labels(
    graph: &ProjectGraph,
    rows: &[RailRow],
) -> std::collections::HashMap<NodeId, String> {
    let mut groups: std::collections::HashMap<String, Vec<NodeId>> =
        std::collections::HashMap::new();
    for row in rows {
        groups
            .entry(raw_label(graph, row))
            .or_default()
            .push(row.id().clone());
    }

    let mut out = std::collections::HashMap::new();
    for (name, ids) in groups {
        if ids.len() < 2 {
            if let Some(id) = ids.into_iter().next() {
                out.insert(id, name);
            }
            continue;
        }

        let chains: std::collections::HashMap<NodeId, Vec<String>> = ids
            .iter()
            .map(|id| (id.clone(), ancestor_names(graph, id)))
            .collect();
        let max_depth = chains.values().map(Vec::len).max().unwrap_or(0);

        let mut depth = 1;
        loop {
            let candidates: Vec<(NodeId, String)> = ids
                .iter()
                .map(|id| {
                    let chain = &chains[id];
                    let sep = qualifier_separator(id);
                    let mut parts: Vec<&str> =
                        chain.iter().take(depth).rev().map(String::as_str).collect();
                    parts.push(name.as_str());
                    (id.clone(), parts.join(sep))
                })
                .collect();

            let mut seen: HashSet<&str> = HashSet::new();
            let all_unique = candidates
                .iter()
                .all(|(_, label)| seen.insert(label.as_str()));
            if all_unique || depth >= max_depth {
                out.extend(candidates);
                break;
            }
            depth += 1;
        }
    }
    out
}

/// Translate `edges` onto the visible row set: each edge's endpoints are
/// mapped through [`effective_row_id`] (a drawn node absorbed into a
/// collapsed namespace reports that namespace's id instead of its own), a
/// self-edge produced by both endpoints collapsing into the *same* namespace
/// is dropped (nothing to rail -- it's now an edge from a row to itself),
/// and the result is deduped so a namespace absorbing many internal cross-
/// edges to another collapsed (or plain) row still contributes exactly one
/// rail between them. This is the edge set [`crate::graph::rails::compute`]
/// should be called with once `collapsed` is non-empty.
pub fn collapse_edges(
    graph: &ProjectGraph,
    edges: &[DepEdge],
    collapsed: &HashSet<NodeId>,
) -> Vec<(NodeId, NodeId)> {
    let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
    let mut out = Vec::new();
    for edge in edges {
        let from = effective_row_id(graph, &edge.from, collapsed);
        let to = effective_row_id(graph, &edge.to, collapsed);
        if from == to {
            continue;
        }
        let pair = (from, to);
        if seen.insert(pair.clone()) {
            out.push(pair);
        }
    }
    out
}

/// The first drawn (file-backed) descendant of `id`, respecting any nested
/// fold: if `id` (or one of its descendants) is itself in `collapsed`, that
/// ancestor's own id is returned rather than descending past it -- used by
/// `crate::core::app::expand_focused_namespace` to re-seat focus onto
/// whatever ends up visible immediately after an expand, mirroring the
/// GUI's `toggle_tests` re-seat precedent for a fold operation that can drop
/// the currently focused row out of existence. Walks children in
/// [`ProjectGraph::sorted_children`] order (name-sorted, matching every
/// other traversal in this crate) so the result is deterministic. `None` if
/// `id` has no drawn descendant at all (shouldn't happen for a real
/// namespace, but this stays total).
pub fn first_visible_descendant(
    graph: &ProjectGraph,
    id: &NodeId,
    collapsed: &HashSet<NodeId>,
) -> Option<NodeId> {
    if collapsed.contains(id) {
        return Some(id.clone());
    }
    let node = graph.node(id)?;
    if node.children.is_empty() {
        return if node.files.is_empty() {
            None
        } else {
            Some(id.clone())
        };
    }
    graph
        .sorted_children(id)
        .iter()
        .find_map(|child| first_visible_descendant(graph, child, collapsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepKind, ModuleNode};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn leaf(id: &str, name: &str, parent: Option<&str>, status: GitStatus) -> (NodeId, ModuleNode) {
        let node_id = NodeId::from(id);
        (
            node_id.clone(),
            ModuleNode {
                id: node_id,
                display_name: name.to_string(),
                parent: parent.map(NodeId::from),
                children: vec![],
                status,
                files: vec![crate::graph::model::FileRef {
                    path: PathBuf::from(format!("{id}.rs")),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        )
    }

    fn namespace(
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

    /// Like [`namespace`], but the namespace node itself is also drawn
    /// (has a backing file) -- the "real defmodule takes precedence over
    /// synthetic namespace" shape from `crate::graph::builder`'s own tests,
    /// e.g. a real `defmodule AppWeb do end` that also has `AppWeb.Foo`/
    /// `AppWeb.Bar` submodules. Exercises the case where the namespace's
    /// own drawn entry sits in `layers` right alongside its descendants.
    fn drawn_namespace(
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
                files: vec![crate::graph::model::FileRef {
                    path: PathBuf::from(format!("{id}.ex")),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        )
    }

    /// Two namespaces (`ns_a` with children `a1`/`a2`, `ns_b` with child
    /// `b1`), plus a cross-namespace dependency edge `a1 -> b1` and an
    /// intra-namespace one `a2 -> a1` (so collapsing `ns_a` drops the
    /// latter as a self-edge but must keep the former, retargeted to
    /// `ns_a`/`ns_b`).
    fn graph_fixture() -> ProjectGraph {
        let (ns_a_id, ns_a) = namespace("ns_a", "NsA", None, &["a1", "a2"]);
        let (a1_id, a1) = leaf("a1", "a1", Some("ns_a"), GitStatus::Modified);
        let (a2_id, a2) = leaf("a2", "a2", Some("ns_a"), GitStatus::Unchanged);
        let (ns_b_id, ns_b) = namespace("ns_b", "NsB", None, &["b1"]);
        let (b1_id, b1) = leaf("b1", "b1", Some("ns_b"), GitStatus::Added);

        let mut nodes = HashMap::new();
        nodes.insert(ns_a_id.clone(), ns_a);
        nodes.insert(a1_id.clone(), a1);
        nodes.insert(a2_id.clone(), a2);
        nodes.insert(ns_b_id.clone(), ns_b);
        nodes.insert(b1_id.clone(), b1);

        ProjectGraph {
            roots: vec![ns_a_id, ns_b_id],
            nodes,
            edges: vec![
                DepEdge {
                    from: a1_id.clone(),
                    to: b1_id.clone(),
                    kind: DepKind::Use,
                },
                DepEdge {
                    from: a2_id,
                    to: a1_id,
                    kind: DepKind::Alias,
                },
            ],
        }
    }

    fn layers_fixture() -> Vec<Vec<NodeId>> {
        vec![
            vec![NodeId::from("a1"), NodeId::from("a2")],
            vec![NodeId::from("b1")],
        ]
    }

    #[test]
    fn fully_expanded_by_default_yields_one_node_row_per_layer_entry() {
        let g = graph_fixture();
        let layers = layers_fixture();
        let rows = visible_rows(&g, &layers, &HashSet::new());

        assert_eq!(
            rows,
            vec![
                RailRow::Node(NodeId::from("a1")),
                RailRow::Node(NodeId::from("a2")),
                RailRow::Node(NodeId::from("b1")),
            ]
        );
    }

    #[test]
    fn collapsing_a_namespace_replaces_its_members_with_one_row() {
        let g = graph_fixture();
        let layers = layers_fixture();
        let collapsed = HashSet::from([NodeId::from("ns_a")]);
        let rows = visible_rows(&g, &layers, &collapsed);

        assert_eq!(
            rows,
            vec![
                RailRow::Collapsed {
                    namespace: NodeId::from("ns_a"),
                    module_count: 2,
                    changed_count: 1,
                },
                RailRow::Node(NodeId::from("b1")),
            ]
        );
    }

    #[test]
    fn collapsing_both_namespaces_yields_two_collapsed_rows() {
        let g = graph_fixture();
        let layers = layers_fixture();
        let collapsed = HashSet::from([NodeId::from("ns_a"), NodeId::from("ns_b")]);
        let rows = visible_rows(&g, &layers, &collapsed);

        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], RailRow::Collapsed { .. }));
        assert!(matches!(rows[1], RailRow::Collapsed { .. }));
    }

    /// Real-world shape from a `--tui` bug report: a namespace that is
    /// itself a drawn node (a real `defmodule AppWeb do end`, per
    /// `crate::graph::builder`'s "real defmodule takes precedence over
    /// synthetic namespace" precedent) sits in `layers` alongside its own
    /// descendants. Collapsing it must absorb the namespace's own drawn
    /// entry into the single [`RailRow::Collapsed`] row too, not also emit
    /// a separate [`RailRow::Node`] for it -- `collapse_root` only walks
    /// *ancestors*, so without an explicit "is `id` itself collapsed?"
    /// check the namespace's own layer entry falls through to the `None`
    /// arm and renders a second, duplicate row for the same [`NodeId`].
    #[test]
    fn collapsing_a_namespace_that_is_itself_drawn_yields_only_one_row_for_it() {
        let (ns_id, ns) = drawn_namespace("ns_a", "NsA", None, &["a1"]);
        let (a1_id, a1) = leaf("a1", "a1", Some("ns_a"), GitStatus::Modified);

        let mut nodes = HashMap::new();
        nodes.insert(ns_id.clone(), ns);
        nodes.insert(a1_id.clone(), a1);
        let g = ProjectGraph {
            roots: vec![ns_id.clone()],
            nodes,
            edges: vec![],
        };
        // The namespace's own drawn entry sits in `layers` right alongside
        // its child, exactly as `crate::graph::layers::assign_layers` would
        // place two unrelated drawn nodes.
        let layers = vec![vec![ns_id.clone(), a1_id]];
        let collapsed = HashSet::from([ns_id.clone()]);

        let rows = visible_rows(&g, &layers, &collapsed);

        assert_eq!(
            rows,
            vec![RailRow::Collapsed {
                namespace: ns_id,
                module_count: 2,
                changed_count: 1,
            }],
            "expected exactly one collapsed row absorbing both the namespace's own entry and its child, not a duplicate Node row"
        );
    }

    /// Real-world shape: two distinct file-backed `docs` directories
    /// (`file:docs` at the repo root, `file:priv/static/docs` nested three
    /// levels down) both visible as plain [`RailRow::Node`]s -- their
    /// `display_name`s collide even though the ids are unrelated. The
    /// root-level one has no parent at all, so it can never gain a prefix;
    /// disambiguation must still make the *other* one distinct from it.
    #[test]
    fn disambiguated_labels_qualifies_colliding_file_nodes_with_a_slash_path() {
        let (root_docs_id, root_docs) = leaf("file:docs", "docs", None, GitStatus::Unchanged);
        let (priv_id, priv_ns) = namespace("file:priv", "priv", None, &["file:priv/static"]);
        let (static_id, static_ns) = namespace(
            "file:priv/static",
            "static",
            Some("file:priv"),
            &["file:priv/static/docs"],
        );
        let (nested_docs_id, nested_docs) = leaf(
            "file:priv/static/docs",
            "docs",
            Some("file:priv/static"),
            GitStatus::Unchanged,
        );

        let mut nodes = HashMap::new();
        nodes.insert(root_docs_id.clone(), root_docs);
        nodes.insert(priv_id.clone(), priv_ns);
        nodes.insert(static_id, static_ns);
        nodes.insert(nested_docs_id.clone(), nested_docs);
        let g = ProjectGraph {
            roots: vec![root_docs_id.clone(), priv_id],
            nodes,
            edges: vec![],
        };

        let rows = vec![
            RailRow::Node(root_docs_id.clone()),
            RailRow::Node(nested_docs_id.clone()),
        ];
        let labels = disambiguated_labels(&g, &rows);

        assert_eq!(labels.get(&root_docs_id), Some(&"docs".to_string()));
        assert_eq!(
            labels.get(&nested_docs_id),
            Some(&"static/docs".to_string())
        );
    }

    /// Two distinct `Auction` namespaces under different Elixir parents --
    /// one folded (so it renders as a [`RailRow::Collapsed`] summary), one
    /// a plain drawn module. Disambiguation must key off [`RailRow::id`]'s
    /// bare name regardless of which row kind it renders as, and use `.`
    /// (not `/`) for a non-file id.
    #[test]
    fn disambiguated_labels_qualifies_colliding_elixir_namespaces_with_a_dot_path() {
        let (foo_id, foo) = namespace("elixir:Foo", "Foo", None, &["elixir:Foo.Auction"]);
        let (foo_auction_id, foo_auction) =
            namespace("elixir:Foo.Auction", "Auction", Some("elixir:Foo"), &[]);
        let (bar_auction_id, bar_auction) = leaf(
            "elixir:Bar.Auction",
            "Auction",
            Some("elixir:Bar"),
            GitStatus::Modified,
        );
        let (bar_id, bar) = namespace("elixir:Bar", "Bar", None, &["elixir:Bar.Auction"]);

        let mut nodes = HashMap::new();
        nodes.insert(foo_id.clone(), foo);
        nodes.insert(foo_auction_id.clone(), foo_auction);
        nodes.insert(bar_auction_id.clone(), bar_auction);
        nodes.insert(bar_id, bar);
        let g = ProjectGraph {
            roots: vec![foo_id, bar_auction_id.clone()],
            nodes,
            edges: vec![],
        };

        let rows = vec![
            RailRow::Collapsed {
                namespace: foo_auction_id.clone(),
                module_count: 0,
                changed_count: 0,
            },
            RailRow::Node(bar_auction_id.clone()),
        ];
        let labels = disambiguated_labels(&g, &rows);

        assert_eq!(
            labels.get(&foo_auction_id),
            Some(&"Foo.Auction".to_string())
        );
        assert_eq!(
            labels.get(&bar_auction_id),
            Some(&"Bar.Auction".to_string())
        );
    }

    #[test]
    fn disambiguated_labels_leaves_a_unique_name_unqualified() {
        let (a1_id, a1) = leaf("a1", "a1", None, GitStatus::Unchanged);
        let mut nodes = HashMap::new();
        nodes.insert(a1_id.clone(), a1);
        let g = ProjectGraph {
            roots: vec![a1_id.clone()],
            nodes,
            edges: vec![],
        };

        let rows = vec![RailRow::Node(a1_id.clone())];
        let labels = disambiguated_labels(&g, &rows);

        assert_eq!(labels.get(&a1_id), Some(&"a1".to_string()));
    }

    /// A name colliding with something currently folded out of view (not
    /// present in `rows` at all) isn't a collision the user can actually
    /// see, so it must not be qualified.
    #[test]
    fn disambiguated_labels_ignores_collisions_outside_the_visible_row_set() {
        let (a1_id, a1) = leaf("ns_a::docs", "docs", Some("ns_a"), GitStatus::Unchanged);
        let (b1_id, b1) = leaf("ns_b::docs", "docs", Some("ns_b"), GitStatus::Unchanged);
        let mut nodes = HashMap::new();
        nodes.insert(a1_id.clone(), a1);
        nodes.insert(b1_id, b1);
        let g = ProjectGraph {
            roots: vec![a1_id.clone()],
            nodes,
            edges: vec![],
        };

        // Only `a1_id` is actually visible right now.
        let rows = vec![RailRow::Node(a1_id.clone())];
        let labels = disambiguated_labels(&g, &rows);

        assert_eq!(labels.get(&a1_id), Some(&"docs".to_string()));
    }

    #[test]
    fn collapse_edges_retargets_cross_namespace_edges_and_drops_self_edges() {
        let g = graph_fixture();
        let collapsed = HashSet::from([NodeId::from("ns_a"), NodeId::from("ns_b")]);
        let edges = collapse_edges(&g, &g.edges, &collapsed);

        // a1 -> b1 becomes ns_a -> ns_b; a2 -> a1 becomes ns_a -> ns_a and
        // is dropped as a self-edge.
        assert_eq!(edges, vec![(NodeId::from("ns_a"), NodeId::from("ns_b"))]);
    }

    #[test]
    fn collapse_edges_dedupes_multiple_edges_collapsing_to_the_same_pair() {
        let mut g = graph_fixture();
        // Add a second a1-side member so two distinct raw edges collapse to
        // the same (ns_a, ns_b) pair.
        let (a3_id, a3) = leaf("a3", "a3", Some("ns_a"), GitStatus::Unchanged);
        g.nodes.insert(a3_id.clone(), a3);
        g.nodes
            .get_mut(&NodeId::from("ns_a"))
            .unwrap()
            .children
            .push(a3_id.clone());
        g.edges.push(DepEdge {
            from: a3_id,
            to: NodeId::from("b1"),
            kind: DepKind::Use,
        });

        let collapsed = HashSet::from([NodeId::from("ns_a"), NodeId::from("ns_b")]);
        let edges = collapse_edges(&g, &g.edges, &collapsed);

        assert_eq!(edges, vec![(NodeId::from("ns_a"), NodeId::from("ns_b"))]);
    }

    #[test]
    fn collapse_edges_is_identity_with_nothing_collapsed() {
        let g = graph_fixture();
        let edges = collapse_edges(&g, &g.edges, &HashSet::new());

        assert_eq!(
            edges,
            vec![
                (NodeId::from("a1"), NodeId::from("b1")),
                (NodeId::from("a2"), NodeId::from("a1")),
            ]
        );
    }

    #[test]
    fn first_visible_descendant_finds_the_name_first_drawn_child() {
        let g = graph_fixture();
        let found = first_visible_descendant(&g, &NodeId::from("ns_a"), &HashSet::new());
        assert_eq!(found, Some(NodeId::from("a1")));
    }

    #[test]
    fn first_visible_descendant_stops_at_a_nested_collapsed_namespace() {
        // ns_outer contains ns_a (collapsed) -- expanding ns_outer must
        // re-seat onto ns_a's own collapsed row, not descend into a1/a2.
        let mut g = graph_fixture();
        let (ns_outer_id, ns_outer) = namespace("ns_outer", "Outer", None, &["ns_a"]);
        g.nodes.get_mut(&NodeId::from("ns_a")).unwrap().parent = Some(ns_outer_id.clone());
        g.nodes.insert(ns_outer_id.clone(), ns_outer);

        let collapsed = HashSet::from([NodeId::from("ns_a")]);
        let found = first_visible_descendant(&g, &ns_outer_id, &collapsed);
        assert_eq!(found, Some(NodeId::from("ns_a")));
    }

    #[test]
    fn first_visible_descendant_none_for_an_empty_namespace() {
        let (empty_id, empty_ns) = namespace("empty", "Empty", None, &[]);
        let mut nodes = HashMap::new();
        nodes.insert(empty_id.clone(), empty_ns);
        let g = ProjectGraph {
            roots: vec![empty_id.clone()],
            nodes,
            edges: vec![],
        };
        assert_eq!(
            first_visible_descendant(&g, &empty_id, &HashSet::new()),
            None
        );
    }
}
