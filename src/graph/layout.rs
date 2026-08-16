//! Pure recursive shelf-packing layout: turns a [`ProjectGraph`] into
//! absolute-position rectangles for every node. Zero dependencies on
//! egui/git2 -- this module owns its own minimal geometry newtypes
//! ([`Pos`], [`Size`], [`Rect`]) rather than pulling in a GUI crate's
//! vector types.
//!
//! Layout is bottom-up: a leaf gets a fixed [`LEAF_W`]x[`LEAF_H`] box; a
//! parent packs its children (in [`ProjectGraph::sorted_children`] order,
//! the same order navigation uses) onto shelves left-to-right, wrapping
//! when a row would exceed a target width, then wraps that packed area in
//! [`MARGIN`] on every side and a [`TITLE_H`] label strip on top. See
//! "Layout algorithm" in the project plan for the full rationale.

use std::collections::HashMap;

use crate::graph::model::{NodeId, ProjectGraph};

/// Fixed width of a leaf (childless) node's box.
pub const LEAF_W: f32 = 120.0;
/// Fixed height of a leaf (childless) node's box.
pub const LEAF_H: f32 = 60.0;
/// Gap left between sibling boxes on the same shelf, and between shelves.
pub const PADDING: f32 = 8.0;
/// Inset between a container box's border and its packed children.
pub const MARGIN: f32 = 16.0;
/// Height of the label strip reserved at the top of a container box, above
/// its children (in addition to [`MARGIN`]).
pub const TITLE_H: f32 = 20.0;

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

/// The full computed layout: every node's absolute rect.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutResult {
    pub rects: HashMap<NodeId, Rect>,
}

/// Lay out an entire project graph: recursively size every node bottom-up,
/// pack children onto shelves at every level (roots included), then
/// translate to absolute coordinates.
pub fn layout(graph: &ProjectGraph) -> LayoutResult {
    let mut sizes = HashMap::new();
    let mut local_layouts = HashMap::new();
    for root in graph.sorted_roots() {
        compute_size(graph, &root, &mut sizes, &mut local_layouts);
    }

    let root_items: Vec<(NodeId, Size)> = graph
        .sorted_roots()
        .into_iter()
        .map(|id| {
            let size = sizes[&id];
            (id, size)
        })
        .collect();
    let (root_positions, _) = pack_shelves(&root_items);

    let mut rects = HashMap::new();
    for (root_id, pos) in root_positions {
        place_subtree(&root_id, pos, &sizes, &local_layouts, &mut rects);
    }

    LayoutResult { rects }
}

/// Bottom-up: compute (and memoize) `id`'s size, recursing into children
/// first since a parent's size depends on its packed children. Also
/// memoizes the children's local (parent-relative) shelf positions, so
/// [`place_subtree`] doesn't need to re-pack.
fn compute_size(
    graph: &ProjectGraph,
    id: &NodeId,
    sizes: &mut HashMap<NodeId, Size>,
    local_layouts: &mut HashMap<NodeId, Vec<(NodeId, Pos)>>,
) -> Size {
    if let Some(size) = sizes.get(id) {
        return *size;
    }

    let children = graph.sorted_children(id);
    let size = if children.is_empty() {
        Size {
            w: LEAF_W,
            h: LEAF_H,
        }
    } else {
        let items: Vec<(NodeId, Size)> = children
            .iter()
            .map(|child| {
                let child_size = compute_size(graph, child, sizes, local_layouts);
                (child.clone(), child_size)
            })
            .collect();
        let (positions, content_size) = pack_shelves(&items);
        local_layouts.insert(id.clone(), positions);
        Size {
            w: content_size.w + 2.0 * MARGIN,
            h: content_size.h + 2.0 * MARGIN + TITLE_H,
        }
    };

    sizes.insert(id.clone(), size);
    size
}

/// Top-down: place `id`'s rect at absolute `origin`, then recurse into its
/// children (if any) using the local positions memoized by
/// [`compute_size`], offset by the container's margin and title strip.
fn place_subtree(
    id: &NodeId,
    origin: Pos,
    sizes: &HashMap<NodeId, Size>,
    local_layouts: &HashMap<NodeId, Vec<(NodeId, Pos)>>,
    rects: &mut HashMap<NodeId, Rect>,
) {
    let size = sizes[id];
    rects.insert(id.clone(), Rect { origin, size });

    let Some(children) = local_layouts.get(id) else {
        return;
    };
    for (child_id, local_pos) in children {
        let child_origin = Pos {
            x: origin.x + MARGIN + local_pos.x,
            y: origin.y + TITLE_H + MARGIN + local_pos.y,
        };
        place_subtree(child_id, child_origin, sizes, local_layouts, rects);
    }
}

/// Pack `items` (in caller-supplied order, which callers keep name-sorted)
/// onto shelves: place left-to-right with [`PADDING`] gaps, wrapping to a
/// new shelf once the current row would exceed a target width chosen to
/// keep the packed area roughly square. Returns each item's position
/// relative to `(0, 0)`, plus the bounding size of the packed content
/// (no margin/title included).
fn pack_shelves(items: &[(NodeId, Size)]) -> (Vec<(NodeId, Pos)>, Size) {
    if items.is_empty() {
        return (Vec::new(), Size { w: 0.0, h: 0.0 });
    }

    let total_area: f32 = items.iter().map(|(_, s)| s.w * s.h).sum();
    let target_width = f32::max(LEAF_W, total_area.sqrt() * 1.3);

    let mut positions = Vec::with_capacity(items.len());
    let mut cursor_x = 0.0_f32;
    let mut cursor_y = 0.0_f32;
    let mut shelf_height = 0.0_f32;
    let mut content_w = 0.0_f32;

    for (id, size) in items {
        if cursor_x > 0.0 && cursor_x + size.w > target_width {
            cursor_y += shelf_height + PADDING;
            cursor_x = 0.0;
            shelf_height = 0.0;
        }

        positions.push((
            id.clone(),
            Pos {
                x: cursor_x,
                y: cursor_y,
            },
        ));
        content_w = f32::max(content_w, cursor_x + size.w);
        shelf_height = f32::max(shelf_height, size.h);
        cursor_x += size.w + PADDING;
    }

    let content_h = cursor_y + shelf_height;
    (
        positions,
        Size {
            w: content_w,
            h: content_h,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{GitStatus, ModuleNode};
    use std::collections::HashMap as StdHashMap;

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
                files: vec![],
            },
        )
    }

    fn parent(
        id: &str,
        name: &str,
        parent_id: Option<&str>,
        children: &[&str],
    ) -> (NodeId, ModuleNode) {
        let node_id = NodeId::from(id);
        (
            node_id.clone(),
            ModuleNode {
                id: node_id,
                display_name: name.to_string(),
                parent: parent_id.map(NodeId::from),
                children: children.iter().map(|c| NodeId::from(*c)).collect(),
                status: GitStatus::Unchanged,
                files: vec![],
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

    // Milestone 14: leaf sizing + single-level shelf packing.

    #[test]
    fn leaf_only_graph_sizes_every_rect_to_the_leaf_box() {
        let graph = graph_from(
            vec![leaf("a", "a", None), leaf("b", "b", None)],
            vec!["a", "b"],
        );

        let result = layout(&graph);

        for id in ["a", "b"] {
            let rect = result.rects[&NodeId::from(id)];
            assert_eq!(
                rect.size,
                Size {
                    w: LEAF_W,
                    h: LEAF_H
                }
            );
        }
    }

    #[test]
    fn parent_with_three_leaves_packs_children_in_name_order_without_overlap() {
        let graph = graph_from(
            vec![
                parent("p", "p", None, &["p::a", "p::b", "p::c"]),
                leaf("p::a", "a", Some("p")),
                leaf("p::b", "b", Some("p")),
                leaf("p::c", "c", Some("p")),
            ],
            vec!["p"],
        );

        let result = layout(&graph);

        let parent_rect = result.rects[&NodeId::from("p")];
        let a = result.rects[&NodeId::from("p::a")];
        let b = result.rects[&NodeId::from("p::b")];
        let c = result.rects[&NodeId::from("p::c")];

        // Children fit inside the parent (title strip carves off the top,
        // so the parent's own rect -- not some shrunk inner rect -- must
        // still contain each child).
        for child in [&a, &b, &c] {
            assert!(parent_rect.contains(child));
        }

        // No pairwise overlap.
        assert!(!a.intersects(&b));
        assert!(!b.intersects(&c));
        assert!(!a.intersects(&c));

        // Name order matches shelf reading order (top-to-bottom, and
        // left-to-right within a shelf). Note: at this leaf size (120x60),
        // the target-width formula puts each leaf on its own shelf here --
        // see the "many children" test below for a case that packs
        // multiple items onto one shelf.
        assert_in_reading_order(&[a, b, c]);
    }

    /// Assert `rects` (already in name order) are also in shelf reading
    /// order: each next rect is either on the same shelf and further
    /// right, or on a later (strictly lower) shelf.
    fn assert_in_reading_order(rects: &[Rect]) {
        for pair in rects.windows(2) {
            let (prev, next) = (pair[0], pair[1]);
            let same_shelf = prev.origin.y == next.origin.y;
            if same_shelf {
                assert!(prev.origin.x < next.origin.x, "same-shelf order broken");
            } else {
                assert!(prev.origin.y < next.origin.y, "shelf order broken");
            }
        }
    }

    #[test]
    fn many_children_wrap_onto_multiple_shelves_without_overlap() {
        let child_ids: Vec<String> = (0..30).map(|i| format!("p::{i:02}")).collect();
        let child_refs: Vec<&str> = child_ids.iter().map(|s| s.as_str()).collect();

        let mut entries = vec![parent("p", "p", None, &child_refs)];
        for (i, id) in child_ids.iter().enumerate() {
            entries.push(leaf(id, &format!("{i:02}"), Some("p")));
        }
        let graph = graph_from(entries, vec!["p"]);

        let result = layout(&graph);

        let child_rects: Vec<Rect> = child_ids
            .iter()
            .map(|id| result.rects[&NodeId::from(id.as_str())])
            .collect();

        let mut ys: Vec<i64> = child_rects.iter().map(|r| r.origin.y as i64).collect();
        ys.sort();
        ys.dedup();
        assert!(ys.len() >= 2, "expected children to wrap onto >=2 shelves");

        for i in 0..child_rects.len() {
            for j in (i + 1)..child_rects.len() {
                assert!(
                    !child_rects[i].intersects(&child_rects[j]),
                    "children {i} and {j} overlap"
                );
            }
        }
    }

    // Milestone 15: multi-level nesting + absolute positions.

    #[test]
    fn grandchild_is_contained_in_child_is_contained_in_root() {
        let graph = graph_from(
            vec![
                parent("root", "root", None, &["root::mid"]),
                parent("root::mid", "mid", Some("root"), &["root::mid::leaf"]),
                leaf("root::mid::leaf", "leaf", Some("root::mid")),
            ],
            vec!["root"],
        );

        let result = layout(&graph);

        let root_rect = result.rects[&NodeId::from("root")];
        let mid_rect = result.rects[&NodeId::from("root::mid")];
        let leaf_rect = result.rects[&NodeId::from("root::mid::leaf")];

        assert!(root_rect.contains(&mid_rect));
        assert!(mid_rect.contains(&leaf_rect));
    }

    #[test]
    fn disjoint_roots_do_not_intersect() {
        let graph = graph_from(
            vec![
                parent("root_a", "root_a", None, &["root_a::x"]),
                leaf("root_a::x", "x", Some("root_a")),
                parent("root_b", "root_b", None, &["root_b::y"]),
                leaf("root_b::y", "y", Some("root_b")),
            ],
            vec!["root_a", "root_b"],
        );

        let result = layout(&graph);

        let a = result.rects[&NodeId::from("root_a")];
        let b = result.rects[&NodeId::from("root_b")];
        assert!(!a.intersects(&b));
    }

    #[test]
    fn layout_is_deterministic() {
        let graph = graph_from(
            vec![
                parent("root", "root", None, &["root::a", "root::b"]),
                leaf("root::a", "a", Some("root")),
                leaf("root::b", "b", Some("root")),
            ],
            vec!["root"],
        );

        let first = layout(&graph);
        let second = layout(&graph);

        assert_eq!(first.rects, second.rects);
    }
}
