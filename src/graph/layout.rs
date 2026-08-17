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

use crate::graph::layers::assign_layers;
use crate::graph::model::{DepEdge, NodeId, ProjectGraph};

/// Fixed width of a node's box (every drawn node is leaf-sized now -- there
/// are no more container boxes).
pub const LEAF_W: f32 = 120.0;
/// Fixed height of a node's box.
pub const LEAF_H: f32 = 60.0;
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
}

/// Lay out an entire project graph: assign layers, pack each layer into a
/// horizontal band (wrapping onto extra rows if it'd otherwise be too
/// wide), stack bands top-to-bottom with [`BAND_GAP`] between them, then
/// resolve straight-line edge paths.
pub fn layout(graph: &ProjectGraph) -> LayoutResult {
    let layers = assign_layers(graph);

    let mut rects = HashMap::new();
    let mut cursor_y = 0.0_f32;
    for layer in &layers {
        let items: Vec<(NodeId, Size)> = layer
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    Size {
                        w: LEAF_W,
                        h: LEAF_H,
                    },
                )
            })
            .collect();
        let (positions, band_size) = pack_row(&items);
        for (id, pos) in positions {
            rects.insert(
                id,
                Rect {
                    origin: Pos {
                        x: pos.x,
                        y: pos.y + cursor_y,
                    },
                    size: Size {
                        w: LEAF_W,
                        h: LEAF_H,
                    },
                },
            );
        }
        cursor_y += band_size.h + BAND_GAP;
    }

    let edges = layout_edges(&graph.edges, &rects);

    LayoutResult {
        rects,
        edges,
        layers,
    }
}

/// The target row width a band wraps at: at least [`MIN_BAND_WIDTH`], or
/// wider still for a band with enough nodes that a square-ish block would
/// exceed it (same square-area heuristic the old shelf-packer used, just
/// with a floor big enough that small bands don't wrap prematurely).
fn min_band_width(total_area: f32) -> f32 {
    f32::max(MIN_BAND_WIDTH, total_area.sqrt() * 1.3)
}

/// Pack `items` (in caller-supplied order -- for a layer, `assign_layers`'s
/// root-then-name order) onto rows: place left-to-right with [`PADDING`]
/// gaps, wrapping to a new row once the current row would exceed
/// [`min_band_width`]. Returns each item's position relative to `(0, 0)`,
/// plus the bounding size of the packed content.
fn pack_row(items: &[(NodeId, Size)]) -> (Vec<(NodeId, Pos)>, Size) {
    if items.is_empty() {
        return (Vec::new(), Size { w: 0.0, h: 0.0 });
    }

    let total_area: f32 = items.iter().map(|(_, s)| s.w * s.h).sum();
    let target_width = min_band_width(total_area);

    let mut positions = Vec::with_capacity(items.len());
    let mut cursor_x = 0.0_f32;
    let mut cursor_y = 0.0_f32;
    let mut row_height = 0.0_f32;
    let mut content_w = 0.0_f32;

    for (id, size) in items {
        if cursor_x > 0.0 && cursor_x + size.w > target_width {
            cursor_y += row_height + PADDING;
            cursor_x = 0.0;
            row_height = 0.0;
        }

        positions.push((
            id.clone(),
            Pos {
                x: cursor_x,
                y: cursor_y,
            },
        ));
        content_w = f32::max(content_w, cursor_x + size.w);
        row_height = f32::max(row_height, size.h);
        cursor_x += size.w + PADDING;
    }

    let content_h = cursor_y + row_height;
    (
        positions,
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
}
