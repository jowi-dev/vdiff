//! Pure layered-dependency layout: turns a [`ProjectGraph`] into absolute-
//! position rectangles for every *drawn* node (a real module/file node --
//! see [`crate::graph::layers`]) plus straight-line edge paths. Zero
//! dependencies on egui/git2 -- this module owns its own minimal geometry
//! newtypes ([`Pos`], [`Size`], [`Rect`]) rather than pulling in a GUI
//! crate's vector types.
//!
//! Vertical position is dependency depth, not namespace containment: each
//! layer from [`crate::graph::layers::assign_layers`] becomes a horizontal
//! band, layer 0 (nothing depends on it) at the top, edges flowing visually
//! downward. Within a band, nodes are packed left-to-right in the layer's
//! given order (already root-then-name sorted by `assign_layers`), wrapping
//! onto additional rows within the same band if the row would grow wider
//! than [`min_band_width`] lets it -- see [`pack_row`]. There is no more
//! nesting: namespace containment is conveyed by color/label in the UI
//! layer, not by box-in-box geometry.

use std::collections::HashMap;

use crate::graph::labels::abbreviated_label;
use crate::graph::layers::assign_layers;
use crate::graph::model::{DepEdge, NodeId, ProjectGraph};
use crate::graph::test_modules::TestStrip;

/// Floor on a node's box width -- also the width used for a short label
/// (see [`node_size`]).
pub const LEAF_W: f32 = 120.0;
/// Ceiling on a node's box width, so a long fully-qualified label still
/// wraps rather than growing the box without bound.
pub const MAX_LEAF_W: f32 = 280.0;
/// Fixed height of a node's box.
pub const LEAF_H: f32 = 60.0;
/// Estimated pixel width of one character at a 12px proportional font --
/// used both to size a node's box to its label (see [`node_size`]) and, in
/// [`crate::ui::graph_view`], to decide when the painted label needs
/// truncating. An estimate, not a real text measurement, but the same
/// estimate on both sides keeps sizing and truncation consistent with each
/// other.
pub const CHAR_W: f32 = 7.2;
/// Horizontal padding kept on each side of a label inside its box.
pub const TEXT_PAD: f32 = 8.0;
/// Gap left between sibling boxes on the same row, and between rows within
/// a band.
pub const PADDING: f32 = 8.0;
/// Vertical gap between one layer's band and the next -- generous enough
/// that edges crossing between bands stay visually readable rather than
/// running edge-to-edge.
pub const BAND_GAP: f32 = 48.0;
/// Floor on the target row width used to decide when a band wraps onto a
/// new row (see [`min_band_width`]): even a band with very few, very small
/// nodes gets at least this much horizontal room before wrapping, so a
/// two-node band doesn't wrap into a needlessly tall single column.
pub const MIN_BAND_WIDTH: f32 = 1200.0;
/// Extra height added to a node's box when it has an attached
/// [`TestStrip`] (see [`layout_with_test_strips`]) -- a distinct bottom
/// section for the matched test module's short name and status, on top of
/// the node's own [`LEAF_H`].
pub const TEST_STRIP_H: f32 = 20.0;

/// A 2D point in layout space (origin top-left, y grows downward).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pos {
    pub x: f32,
    pub y: f32,
}

/// A width/height pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

/// An axis-aligned box: `origin` is the top-left corner, `size` extends
/// right and down from it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub origin: Pos,
    pub size: Size,
}

impl Rect {
    /// The rect's center point.
    pub fn center(&self) -> Pos {
        Pos {
            x: self.origin.x + self.size.w / 2.0,
            y: self.origin.y + self.size.h / 2.0,
        }
    }

    /// Whether `other` lies entirely within `self` (edges may touch).
    pub fn contains(&self, other: &Rect) -> bool {
        other.origin.x >= self.origin.x
            && other.origin.y >= self.origin.y
            && other.origin.x + other.size.w <= self.origin.x + self.size.w
            && other.origin.y + other.size.h <= self.origin.y + self.size.h
    }

    /// Whether `self` and `other` overlap (touching edges don't count as
    /// overlap -- two boxes sharing a border with no area in common are
    /// treated as non-intersecting).
    pub fn intersects(&self, other: &Rect) -> bool {
        self.origin.x < other.origin.x + other.size.w
            && other.origin.x < self.origin.x + self.size.w
            && self.origin.y < other.origin.y + other.size.h
            && other.origin.y < self.origin.y + self.size.h
    }
}

/// A straight-line edge overlay from the center of one node's rect to the
/// center of another's (v1 rendering; no routing around obstacles).
#[derive(Debug, Clone, PartialEq)]
pub struct EdgePath {
    pub from: NodeId,
    pub to: NodeId,
    pub points: [Pos; 2],
}

/// The full computed layout: every drawn node's absolute rect, one
/// [`EdgePath`] per resolvable [`DepEdge`], and the layer structure used to
/// place them (exactly [`assign_layers`]'s output) so navigation
/// ([`crate::core::focus`]) and rendering agree on which nodes sit in which
/// layer/row without recomputing it.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutResult {
    pub rects: HashMap<NodeId, Rect>,
    pub edges: Vec<EdgePath>,
    pub layers: Vec<Vec<NodeId>>,
    /// Every *visual* row across the whole layout, top-to-bottom, each in
    /// left-to-right order: unlike `layers`, a layer that wrapped onto
    /// multiple sub-rows (see [`pack_rows`]) contributes one entry per
    /// sub-row here, not one entry for the whole layer. This is what lets
    /// [`crate::core::focus::move_focus`]'s `j`/`k` navigate by the row the
    /// user actually sees instead of jumping a whole wrapped layer at once
    /// -- see [`rows_with_x_centers`], which is what actually gets threaded
    /// into `core::App` (as plain ids + x-centers, not `Rect`s, keeping
    /// `core` geometry-free beyond that).
    pub rows: Vec<Vec<NodeId>>,
}

/// `layout.rows` paired with each node's rect x-center, in the same
/// row/left-to-right order -- the shape [`crate::core::focus::move_focus`]
/// actually consults for row-based `j`/`k` (see [`LayoutResult::rows`]'s
/// doc). A node absent from `layout.rects` (shouldn't happen for a drawn
/// node) is skipped rather than panicking.
pub fn rows_with_x_centers(layout: &LayoutResult) -> Vec<Vec<(NodeId, f32)>> {
    layout
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .filter_map(|id| {
                    let center_x = layout.rects.get(id)?.center().x;
                    Some((id.clone(), center_x))
                })
                .collect()
        })
        .collect()
}

/// Lay out an entire project graph: assign layers, pack each layer into a
/// horizontal band (wrapping onto extra rows if it'd otherwise be too
/// wide), center every row -- band or wrapped sub-row alike -- within the
/// graph's overall width (the widest row anywhere in the layout), stack
/// bands top-to-bottom with [`BAND_GAP`] between them, then resolve
/// straight-line edge paths.
///
/// Centering (rather than left-aligning every row at `x=0`, the old
/// behavior) is what makes the graph read as a centered pyramid/stack
/// instead of a ragged left edge: a narrow band sitting under a much wider
/// one no longer looks accidentally offset from it.
///
/// Convenience wrapper around [`layout_with_test_strips`] for every caller
/// that doesn't need the taller combined test-module boxes (most of this
/// module's own tests, and any layout pass over a graph with
/// [`crate::core::app::App::show_tests`] off).
pub fn layout(graph: &ProjectGraph) -> LayoutResult {
    layout_with_test_strips(graph, &HashMap::new())
}

/// [`layout`], but every node id present in `test_strips` gets a taller box
/// ([`LEAF_H`] + [`TEST_STRIP_H`]) to make room for the attached test-module
/// strip [`crate::ui::graph_view`] paints on it -- see
/// [`crate::graph::test_modules::test_strips`], which is what a caller
/// builds `test_strips` from (only ever non-empty when `show_tests` is on;
/// pass an empty map otherwise, same as plain [`layout`]).
pub fn layout_with_test_strips(
    graph: &ProjectGraph,
    test_strips: &HashMap<NodeId, TestStrip>,
) -> LayoutResult {
    layout_from_layers(graph, assign_layers(graph), test_strips)
}

/// [`layout_with_test_strips`], but placing `layers` as given instead of
/// running [`assign_layers`] over `graph` again. For a caller that already
/// holds the layer structure -- [`crate::core::app::App::layers`], rebuilt
/// by the reducer whenever it changes shape (see
/// [`crate::core::app::Cmd::Relayout`]) -- this makes that copy the single
/// source instead of a second derivation that only stays equal to this
/// one's by `assign_layers` being deterministic. The returned
/// [`LayoutResult::layers`] is exactly the `layers` passed in.
pub fn layout_from_layers(
    graph: &ProjectGraph,
    layers: Vec<Vec<NodeId>>,
    test_strips: &HashMap<NodeId, TestStrip>,
) -> LayoutResult {
    let bands: Vec<(Vec<Row>, Size)> = layers
        .iter()
        .map(|layer| {
            let items: Vec<(NodeId, Size)> = layer
                .iter()
                .map(|id| (id.clone(), node_size(graph, id, test_strips)))
                .collect();
            pack_rows(&items)
        })
        .collect();

    let graph_width = bands
        .iter()
        .flat_map(|(rows, _)| rows.iter().map(|row| row.width))
        .fold(0.0_f32, f32::max);

    let mut rects = HashMap::new();
    let mut visual_rows: Vec<Vec<NodeId>> = Vec::new();
    let mut cursor_y = 0.0_f32;
    for (rows, band_size) in &bands {
        for row in rows {
            let shift_x = (graph_width - row.width) / 2.0;
            let mut row_ids = Vec::with_capacity(row.items.len());
            for (id, pos, size) in &row.items {
                rects.insert(
                    id.clone(),
                    Rect {
                        origin: Pos {
                            x: pos.x + shift_x,
                            y: pos.y + cursor_y,
                        },
                        size: *size,
                    },
                );
                row_ids.push(id.clone());
            }
            visual_rows.push(row_ids);
        }
        cursor_y += band_size.h + BAND_GAP;
    }

    let edges = layout_edges(&graph.edges, &rects);

    LayoutResult {
        rects,
        edges,
        layers,
        rows: visual_rows,
    }
}

/// A node's box size: width clamped to fit its painted label (the same
/// [`abbreviated_label`] [`crate::ui::graph_view`] draws) --
/// `label_char_count * CHAR_W + 2*TEXT_PAD`, floored at [`LEAF_W`] and
/// capped at [`MAX_LEAF_W`]. Height is [`LEAF_H`], plus [`TEST_STRIP_H`] if
/// `id` has an attached [`TestStrip`] in `test_strips` (see
/// [`layout_with_test_strips`]). Falls back to the bare id string for the
/// label if `id` isn't in `graph` (shouldn't happen for a drawn node, but
/// keeps this total).
fn node_size(graph: &ProjectGraph, id: &NodeId, test_strips: &HashMap<NodeId, TestStrip>) -> Size {
    let root_id = graph.top_level_root(id);
    let label = match graph.node(id) {
        Some(node) => abbreviated_label(&id.to_string(), &root_id.to_string(), &node.display_name),
        None => id.to_string(),
    };
    let width = (label.chars().count() as f32 * CHAR_W + 2.0 * TEXT_PAD).clamp(LEAF_W, MAX_LEAF_W);
    let height = if test_strips.contains_key(id) {
        LEAF_H + TEST_STRIP_H
    } else {
        LEAF_H
    };
    Size {
        w: width,
        h: height,
    }
}

/// The target row width a band wraps at: at least [`MIN_BAND_WIDTH`], or
/// wider still for a band with enough nodes that a square-ish block would
/// exceed it (same square-area heuristic the old shelf-packer used, just
/// with a floor big enough that small bands don't wrap prematurely).
fn min_band_width(total_area: f32) -> f32 {
    f32::max(MIN_BAND_WIDTH, total_area.sqrt() * 1.3)
}

/// One packed row within a band: its items positioned relative to `(0, 0)`
/// (`y` already reflects which sub-row within the band this is, from
/// wrapping), plus its own content width -- used by [`layout`] to center
/// each row independently within the graph's overall width.
struct Row {
    items: Vec<(NodeId, Pos, Size)>,
    width: f32,
}

/// Pack `items` (in caller-supplied order -- for a layer, `assign_layers`'s
/// root-then-name order) onto rows: place left-to-right with [`PADDING`]
/// gaps, wrapping to a new row once the current row would exceed
/// [`min_band_width`]. Returns each wrapped row (left-aligned at `x=0`,
/// [`layout`] centers them afterward) plus the bounding size of the packed
/// content.
fn pack_rows(items: &[(NodeId, Size)]) -> (Vec<Row>, Size) {
    if items.is_empty() {
        return (Vec::new(), Size { w: 0.0, h: 0.0 });
    }

    let total_area: f32 = items.iter().map(|(_, s)| s.w * s.h).sum();
    let target_width = min_band_width(total_area);

    let mut rows: Vec<Row> = Vec::new();
    let mut current_row: Vec<(NodeId, Pos, Size)> = Vec::new();
    let mut cursor_x = 0.0_f32;
    let mut cursor_y = 0.0_f32;
    let mut row_height = 0.0_f32;

    for (id, size) in items {
        if cursor_x > 0.0 && cursor_x + size.w > target_width {
            rows.push(Row {
                items: std::mem::take(&mut current_row),
                width: cursor_x - PADDING,
            });
            cursor_y += row_height + PADDING;
            cursor_x = 0.0;
            row_height = 0.0;
        }

        current_row.push((
            id.clone(),
            Pos {
                x: cursor_x,
                y: cursor_y,
            },
            *size,
        ));
        row_height = f32::max(row_height, size.h);
        cursor_x += size.w + PADDING;
    }
    rows.push(Row {
        items: current_row,
        width: cursor_x - PADDING,
    });

    let content_h = cursor_y + row_height;
    let content_w = rows.iter().map(|row| row.width).fold(0.0_f32, f32::max);
    (
        rows,
        Size {
            w: content_w,
            h: content_h,
        },
    )
}

/// Resolve every [`DepEdge`] to a straight center-to-center [`EdgePath`],
/// skipping edges whose endpoints have no rect (an endpoint that's a
/// synthetic namespace node, never drawn, or -- defensively -- unknown).
fn layout_edges(dep_edges: &[DepEdge], rects: &HashMap<NodeId, Rect>) -> Vec<EdgePath> {
    dep_edges
        .iter()
        .filter_map(|edge| {
            let from_rect = rects.get(&edge.from)?;
            let to_rect = rects.get(&edge.to)?;
            Some(EdgePath {
                from: edge.from.clone(),
                to: edge.to.clone(),
                points: [from_rect.center(), to_rect.center()],
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepKind, GitStatus, ModuleNode};
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;

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
                files: vec![crate::graph::model::FileRef {
                    path: PathBuf::from(format!("{id}.rs")),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        )
    }

    fn graph_from(entries: Vec<(NodeId, ModuleNode)>, roots: Vec<&str>) -> ProjectGraph {
        let nodes: StdHashMap<NodeId, ModuleNode> = entries.into_iter().collect();
        ProjectGraph {
            nodes,
            roots: roots.into_iter().map(NodeId::from).collect(),
            edges: vec![],
        }
    }

    fn edge(from: &str, to: &str) -> DepEdge {
        DepEdge {
            from: NodeId::from(from),
            to: NodeId::from(to),
            kind: DepKind::Use,
        }
    }

    #[test]
    fn bands_strictly_increase_in_y_between_layers() {
        let mut graph = graph_from(
            vec![
                leaf("a", "a", None),
                leaf("b", "b", None),
                leaf("c", "c", None),
            ],
            vec!["a", "b", "c"],
        );
        graph.edges = vec![edge("a", "b"), edge("b", "c")];

        let result = layout(&graph);

        let a_y = result.rects[&NodeId::from("a")].origin.y;
        let b_y = result.rects[&NodeId::from("b")].origin.y;
        let c_y = result.rects[&NodeId::from("c")].origin.y;

        assert!(a_y < b_y);
        assert!(b_y < c_y);
    }

    #[test]
    fn no_overlaps_within_a_band_even_when_it_wraps() {
        let ids: Vec<String> = (0..40).map(|i| format!("n{i:02}")).collect();
        let entries: Vec<(NodeId, ModuleNode)> = ids.iter().map(|id| leaf(id, id, None)).collect();
        let roots: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let graph = graph_from(entries, roots);

        let result = layout(&graph);

        // All 40 nodes are unconnected -> one trailing layer/band, wide
        // enough (40 * 120px) to force wrapping at MIN_BAND_WIDTH.
        assert_eq!(result.layers.len(), 1);
        let rects: Vec<Rect> = ids
            .iter()
            .map(|id| result.rects[&NodeId::from(id.as_str())])
            .collect();

        let mut ys: Vec<i64> = rects.iter().map(|r| r.origin.y as i64).collect();
        ys.sort();
        ys.dedup();
        assert!(ys.len() >= 2, "expected the band to wrap onto >=2 rows");

        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(!rects[i].intersects(&rects[j]), "nodes {i} and {j} overlap");
            }
        }
    }

    #[test]
    fn layers_field_matches_placement_order() {
        let mut graph = graph_from(
            vec![
                leaf("a", "a", None),
                leaf("b", "b", None),
                leaf("c", "c", None),
            ],
            vec!["a", "b", "c"],
        );
        graph.edges = vec![edge("a", "b"), edge("a", "c")];

        let result = layout(&graph);

        assert_eq!(result.layers, assign_layers(&graph));
        // And every id in `layers` actually got a rect.
        for layer in &result.layers {
            for id in layer {
                assert!(result.rects.contains_key(id));
            }
        }
    }

    #[test]
    fn layout_from_layers_uses_the_given_layers_verbatim() {
        let mut graph = graph_from(
            vec![leaf("a", "a", None), leaf("b", "b", None)],
            vec!["a", "b"],
        );
        graph.edges = vec![edge("a", "b")];

        // Deliberately the REVERSE of what assign_layers would produce for
        // the a -> b edge: if the function re-derived layers itself, "a"
        // would land above "b" and this ordering would flip back.
        let reversed: Vec<Vec<NodeId>> = vec![vec![NodeId::from("b")], vec![NodeId::from("a")]];

        let result = layout_from_layers(&graph, reversed.clone(), &HashMap::new());

        assert_eq!(result.layers, reversed);
        let a_y = result.rects[&NodeId::from("a")].origin.y;
        let b_y = result.rects[&NodeId::from("b")].origin.y;
        assert!(
            b_y < a_y,
            "placement must follow the caller's layers, not a fresh assign_layers pass"
        );
    }

    #[test]
    fn layout_with_test_strips_equals_layout_from_layers_over_assign_layers() {
        let mut graph = graph_from(
            vec![
                leaf("a", "a", None),
                leaf("b", "b", None),
                leaf("c", "c", None),
            ],
            vec!["a", "b", "c"],
        );
        graph.edges = vec![edge("a", "b"), edge("a", "c")];

        let derived = layout_with_test_strips(&graph, &HashMap::new());
        let precomputed = layout_from_layers(&graph, assign_layers(&graph), &HashMap::new());

        assert_eq!(derived, precomputed);
    }

    #[test]
    fn layout_is_deterministic() {
        let mut graph = graph_from(
            vec![leaf("a", "a", None), leaf("b", "b", None)],
            vec!["a", "b"],
        );
        graph.edges = vec![edge("a", "b")];

        let first = layout(&graph);
        let second = layout(&graph);

        assert_eq!(first.rects, second.rects);
        assert_eq!(first.layers, second.layers);
    }

    #[test]
    fn edge_path_runs_center_to_center() {
        let mut graph = graph_from(
            vec![leaf("a", "a", None), leaf("b", "b", None)],
            vec!["a", "b"],
        );
        graph.edges = vec![edge("a", "b")];

        let result = layout(&graph);

        let a_rect = result.rects[&NodeId::from("a")];
        let b_rect = result.rects[&NodeId::from("b")];

        assert_eq!(result.edges.len(), 1);
        assert_eq!(result.edges[0].points, [a_rect.center(), b_rect.center()]);
    }

    #[test]
    fn edge_with_missing_endpoint_is_skipped() {
        let mut graph = graph_from(vec![leaf("a", "a", None)], vec!["a"]);
        graph.edges.push(DepEdge {
            from: NodeId::from("a"),
            to: NodeId::from("missing"),
            kind: DepKind::Use,
        });

        let result = layout(&graph);

        assert!(result.edges.is_empty());
    }

    #[test]
    fn single_leaf_graph_sizes_its_rect_to_the_leaf_box() {
        let graph = graph_from(vec![leaf("a", "a", None)], vec!["a"]);

        let result = layout(&graph);

        let rect = result.rects[&NodeId::from("a")];
        assert_eq!(
            rect.size,
            Size {
                w: LEAF_W,
                h: LEAF_H
            }
        );
    }

    #[test]
    fn a_long_label_widens_the_box_beyond_leaf_w_but_not_past_max_leaf_w() {
        let graph = graph_from(
            vec![leaf(
                "a",
                "a_very_long_display_name_that_should_widen_the_box_a_lot",
                None,
            )],
            vec!["a"],
        );

        let result = layout(&graph);

        let rect = result.rects[&NodeId::from("a")];
        assert!(rect.size.w > LEAF_W, "long label should widen the box");
        assert!(
            rect.size.w <= MAX_LEAF_W,
            "width must not exceed MAX_LEAF_W"
        );
        assert_eq!(rect.size.h, LEAF_H);
    }

    #[test]
    fn a_short_label_keeps_the_box_at_the_leaf_w_floor() {
        let graph = graph_from(vec![leaf("a", "x", None)], vec!["a"]);

        let result = layout(&graph);

        assert_eq!(result.rects[&NodeId::from("a")].size.w, LEAF_W);
    }

    #[test]
    fn rows_field_has_one_entry_per_wrapped_sub_row_not_per_layer() {
        // Same fixture as `no_overlaps_within_a_band_even_when_it_wraps`:
        // 40 unconnected nodes land in a single trailing layer, but wrap
        // onto >=2 visual rows. `layers` has exactly one entry (the whole
        // layer); `rows` must have one entry per wrapped sub-row instead,
        // and every id from the layer must appear in exactly one row.
        let ids: Vec<String> = (0..40).map(|i| format!("n{i:02}")).collect();
        let entries: Vec<(NodeId, ModuleNode)> = ids.iter().map(|id| leaf(id, id, None)).collect();
        let roots: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let graph = graph_from(entries, roots);

        let result = layout(&graph);

        assert_eq!(result.layers.len(), 1, "sanity: one layer");
        assert!(
            result.rows.len() >= 2,
            "expected the layer to wrap onto >=2 visual rows, got {}",
            result.rows.len()
        );

        let mut flattened: Vec<NodeId> = result.rows.iter().flatten().cloned().collect();
        let mut expected: Vec<NodeId> = ids.iter().map(|s| NodeId::from(s.as_str())).collect();
        flattened.sort();
        expected.sort();
        assert_eq!(flattened, expected, "every id appears in exactly one row");
    }

    #[test]
    fn rows_with_x_centers_pairs_each_row_id_with_its_rect_center_x() {
        let mut graph = graph_from(
            vec![leaf("a", "a", None), leaf("b", "b", None)],
            vec!["a", "b"],
        );
        graph.edges = vec![edge("a", "b")];

        let result = layout(&graph);
        let centers = rows_with_x_centers(&result);

        assert_eq!(centers.len(), result.rows.len());
        for (row, center_row) in result.rows.iter().zip(centers.iter()) {
            for (id, (center_id, cx)) in row.iter().zip(center_row.iter()) {
                assert_eq!(id, center_id);
                assert_eq!(*cx, result.rects[id].center().x);
            }
        }
    }

    #[test]
    fn a_node_with_an_attached_test_strip_gets_a_taller_box() {
        let graph = graph_from(vec![leaf("a", "a", None)], vec!["a"]);
        let mut strips = StdHashMap::new();
        strips.insert(
            NodeId::from("a"),
            TestStrip {
                label: "ATest".to_string(),
                status: GitStatus::Modified,
            },
        );

        let result = layout_with_test_strips(&graph, &strips);

        let rect = result.rects[&NodeId::from("a")];
        assert_eq!(rect.size.h, LEAF_H + TEST_STRIP_H);
    }

    #[test]
    fn a_node_without_a_test_strip_keeps_the_plain_leaf_height() {
        let graph = graph_from(vec![leaf("a", "a", None)], vec!["a"]);

        let result = layout_with_test_strips(&graph, &StdHashMap::new());

        let rect = result.rects[&NodeId::from("a")];
        assert_eq!(rect.size.h, LEAF_H);
    }

    #[test]
    fn plain_layout_never_adds_test_strip_height() {
        let graph = graph_from(vec![leaf("a", "a", None)], vec!["a"]);

        let result = layout(&graph);

        assert_eq!(result.rects[&NodeId::from("a")].size.h, LEAF_H);
    }

    #[test]
    fn narrower_rows_are_centered_within_the_widest_row_in_the_graph() {
        // Layer 0: one node with a long label (wide box, ~280px at
        // MAX_LEAF_W). Layer 1: two short-label nodes (~120px each, 248px
        // total row width). Neither row wraps (well under
        // MIN_BAND_WIDTH) -- each layer packs onto exactly one row, and
        // the narrower row (layer 1) must be centered under the wider one
        // (layer 0), not left-aligned at x=0.
        let mut graph = graph_from(
            vec![
                leaf(
                    "a",
                    "a_very_long_display_name_that_should_widen_the_box_a_lot",
                    None,
                ),
                leaf("b", "b", None),
                leaf("c", "c", None),
            ],
            vec!["a", "b", "c"],
        );
        graph.edges = vec![edge("a", "b"), edge("a", "c")];

        let result = layout(&graph);

        let a_rect = result.rects[&NodeId::from("a")];
        let b_rect = result.rects[&NodeId::from("b")];
        let c_rect = result.rects[&NodeId::from("c")];

        assert_eq!(
            a_rect.size.w, MAX_LEAF_W,
            "sanity: layer 0 is the wider row"
        );
        assert_eq!(a_rect.origin.x, 0.0, "the widest row anchors at x=0");

        let layer1_width = c_rect.origin.x + c_rect.size.w - b_rect.origin.x;
        let expected_shift = (a_rect.size.w - layer1_width) / 2.0;
        assert!(expected_shift > 0.0, "sanity: layer 1 really is narrower");
        assert!(
            (b_rect.origin.x - expected_shift).abs() < 0.01,
            "layer 1's row should be centered under layer 0's: b.x={}, expected shift={}",
            b_rect.origin.x,
            expected_shift
        );
    }

    #[test]
    fn variable_width_rows_still_pack_without_overlap() {
        // Mixed short/long labels in one unconnected (trailing) layer --
        // `pack_row` must respect each item's actual width, not a fixed
        // LEAF_W, when deciding wrap points and positions.
        let names = [
            "a",
            "a_moderately_long_name",
            "b",
            "another_pretty_long_display_name_here",
            "c",
        ];
        let entries: Vec<(NodeId, ModuleNode)> = names
            .iter()
            .enumerate()
            .map(|(i, name)| leaf(&format!("n{i}"), name, None))
            .collect();
        let roots: Vec<&str> = (0..names.len()).map(|_| "").collect();
        let mut graph = graph_from(entries, roots);
        graph.roots = (0..names.len())
            .map(|i| NodeId::from(format!("n{i}")))
            .collect();

        let result = layout(&graph);

        let rects: Vec<Rect> = (0..names.len())
            .map(|i| result.rects[&NodeId::from(format!("n{i}"))])
            .collect();
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(!rects[i].intersects(&rects[j]), "nodes {i} and {j} overlap");
            }
        }
    }
}
