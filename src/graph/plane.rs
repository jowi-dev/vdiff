//! Pure, toolkit-neutral nested 2D char-cell layout for the `--tui` "plane"
//! graph screen (the third TUI graph attempt): unlike
//! [`crate::graph::sugiyama`]/[`crate::graph::canvas`], which flatten the
//! visible tree into horizontal Sugiyama bands (rejected in real use --
//! every edge funnels into dense inter-band channel rows), this module lays
//! the same fold-aware visible tree out as actual **nested boxes**, mirroring
//! the GUI's own nested-cluster shape (see `crate::graph::layout`, which
//! does the identical thing in pixels) but in char units and with `[ label ]`
//! text instead of filled boxes.
//!
//! # What "visible tree" means here
//!
//! This module walks [`crate::graph::model::ProjectGraph`] top-down from its
//! roots, using [`crate::graph::model::ModuleNode::children`] directly --
//! *not* [`crate::core::rail_view::visible_rows_with_layers`]'s flattened
//! row list, which only ever contains drawn (file-backed) leaves and never a
//! namespace container (see that module's own doc: [`App::layers`] itself
//! only holds drawn nodes at all). Recursion stops at a node in `collapsed`
//! (an expanded ancestor's box never contains a collapsed descendant's own
//! children -- it renders as a single label row instead, matching
//! [`crate::core::rail_view::RailRow::Collapsed`]'s summary) or a leaf with
//! no children (a real drawn module). Any other node is a namespace and gets
//! expanded into a box containing its own (recursively laid out) children.
//!
//! # Layout algorithm
//!
//! Every node becomes an [`Item`]: either a **leaf** (height 1, width the
//! caller-supplied label's character count) or a **box** (its children
//! packed via [`shelf_pack`], wrapped in a 1-cell padding ring plus a
//! 1-row title border top and bottom). Children -- at every nesting level,
//! including the top level -- are ordered by `(layer index, name)`: the
//! layer index comes from `App::layers` when the id is itself a drawn leaf,
//! or the minimum layer index among its descendants otherwise (see
//! [`layer_index`]), so dependency depth still flows top-to-bottom the way
//! it does in the rail/canvas views, just inside each container rather than
//! across the whole screen.
//!
//! [`shelf_pack`] places same-level siblings left-to-right, wrapping onto a
//! new shelf row once a target width is exceeded -- a *wide, not tall* bias
//! (terminal cells read roughly 2:1 tall/wide), with 2 blank columns between
//! siblings and 1 blank row between shelf rows, deliberately left empty as
//! routing space for [`crate::graph::plane_edges`]. The target width itself
//! scales with the total packed area (`ceil(sqrt(area)) * 2.5`), floored at
//! the single widest child's own width so nothing gets wrapped narrower than
//! it can physically be.
//!
//! Every [`Item`]'s own rect, once placed by its parent's [`shelf_pack`]
//! call, is relative to that parent's own coordinate origin -- a box's
//! children, in turn, are relative to *that box's* origin (`(0, 0)` at its
//! own top-left border corner). [`layout`]'s final pass ([`flatten`]) walks
//! this tree top-down accumulating absolute offsets, so every entry in the
//! returned [`PlaneLayout::rows`]/[`PlaneLayout::boxes`] ends up in one
//! shared, unbounded, absolute char space -- exactly the coordinate system
//! [`crate::graph::plane_edges::route_edges`] and the `--tui` renderer both
//! need.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::graph::model::{NodeId, ProjectGraph};

/// One cell rect in unbounded, absolute char space: `(x, y)` is the
/// top-left corner, `(w, h)` the size. Used both for a single label row
/// and for a whole expanded namespace's box (border included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn x_center(&self) -> f32 {
        self.x as f32 + self.w as f32 / 2.0
    }

    pub fn y_center(&self) -> f32 {
        self.y as f32 + self.h as f32 / 2.0
    }

    fn right(&self) -> usize {
        self.x + self.w
    }

    fn bottom(&self) -> usize {
        self.y + self.h
    }

    /// Whether `self` fully contains `other` (used by this module's own
    /// containment tests -- an expanded namespace's box must strictly
    /// contain every one of its children's rects).
    pub fn contains(&self, other: &Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    /// Whether `self` and `other` occupy any common cell -- used by this
    /// module's own no-overlap tests.
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// Horizontal gap (blank columns) [`shelf_pack`] leaves between adjacent
/// siblings on the same shelf row -- routing space for
/// [`crate::graph::plane_edges`].
const GAP_X: usize = 2;
/// Vertical gap (blank rows) [`shelf_pack`] leaves between shelf rows.
const GAP_Y: usize = 1;
/// Border (1 row/col) + padding (1 row/col) on every side of an expanded
/// namespace's box, wrapping its packed children.
const BOX_MARGIN: usize = 2;
// The "wide, not tall" bias [`shelf_pack`]'s target-width formula applies to
// `ceil(sqrt(total_child_area))` (terminal cells read roughly 2:1 tall/wide,
// so a squarish area budget should read as a wide-and-short rectangle of
// cells, not a literal square) is expressed there as the `5 / 2` integer
// multiply (equivalent to `* 2.5` without pulling `f64` multiplication into
// that otherwise-integer arithmetic).

/// The pure output of [`layout`]: every visible row's own rect (a leaf
/// module or a collapsed-namespace summary row -- what the renderer paints
/// label text into, and what the focus grid navigates over), every expanded
/// namespace's own box rect plus its ordered child id list (what the
/// renderer's nested draw walk needs), and the top-level item order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlaneLayout {
    pub rows: HashMap<NodeId, Rect>,
    pub boxes: HashMap<NodeId, Rect>,
    pub children_of: HashMap<NodeId, Vec<NodeId>>,
    pub top_level: Vec<NodeId>,
    /// The layout's overall bounding size: `width` is the rightmost edge
    /// any rect reaches, `height` the bottommost -- what the renderer's
    /// vertical scroll clamp treats as `total_rows`.
    pub width: usize,
    pub height: usize,
}

/// One item, already sized (and, for a box, already internally packed) but
/// not yet positioned within its own parent -- [`shelf_pack`] fills in
/// `rect.x`/`rect.y` once it knows this item's siblings.
#[derive(Debug, Clone)]
struct Item {
    id: NodeId,
    rect: Rect,
    kind: ItemKind,
}

#[derive(Debug, Clone)]
enum ItemKind {
    Leaf,
    Box(Vec<Item>),
}

/// Lay `graph`'s visible tree out in unbounded, absolute char space. `layers`
/// is `App::layers` (drawn-leaf-only depth layering, used purely as an
/// ordering hint -- see the module doc); `collapsed` is `App::fold_collapsed`
/// (where recursion stops); `leaf_label` returns the exact text a leaf or
/// collapsed-namespace row should render as (the caller -- `crate::tui::
/// render` -- is expected to reuse the same status-glyph/badge text the
/// rail/canvas views already render, so all three views' widths and content
/// agree; see that module's `plane_leaf_label`).
pub fn layout(
    graph: &ProjectGraph,
    layers: &[Vec<NodeId>],
    collapsed: &HashSet<NodeId>,
    leaf_label: impl Fn(&NodeId) -> String,
) -> PlaneLayout {
    let leaf_layer: HashMap<NodeId, usize> = layers
        .iter()
        .enumerate()
        .flat_map(|(idx, row)| row.iter().map(move |id| (id.clone(), idx)))
        .collect();
    let mut order_cache: HashMap<NodeId, usize> = HashMap::new();

    let mut roots = graph.sorted_roots();
    roots.sort_by(|a, b| {
        order_key(graph, a, &leaf_layer, &mut order_cache).cmp(&order_key(
            graph,
            b,
            &leaf_layer,
            &mut order_cache,
        ))
    });
    // `sort_by` above is a secondary sort layered on top of `sorted_roots`'s
    // name order; `sort_by`'s stability keeps that name order as the
    // tie-break for equal layer indices, matching the module doc's
    // `(layer index, name)` ordering exactly.

    let items: Vec<Item> = roots
        .iter()
        .map(|id| {
            build_item(
                graph,
                id,
                collapsed,
                &leaf_layer,
                &mut order_cache,
                &leaf_label,
            )
        })
        .collect();
    let packed = shelf_pack(items);

    let mut out = PlaneLayout {
        top_level: packed.iter().map(|i| i.id.clone()).collect(),
        ..Default::default()
    };
    for item in &packed {
        flatten(item, (0, 0), &mut out);
    }
    out
}

/// `id`'s ordering key: its own layer index if it's a drawn leaf directly
/// present in `leaf_layer`, otherwise the minimum ordering key among its
/// children (recursively), or [`usize::MAX`] if it has no children at all
/// (an isolated, layer-less leaf or an empty namespace) -- sorts last,
/// deterministically, rather than panicking or picking an arbitrary
/// default. Memoized in `cache` since the same namespace's key can be
/// requested multiple times across sibling comparisons.
fn order_key(
    graph: &ProjectGraph,
    id: &NodeId,
    leaf_layer: &HashMap<NodeId, usize>,
    cache: &mut HashMap<NodeId, usize>,
) -> usize {
    if let Some(key) = cache.get(id) {
        return *key;
    }
    let key = if let Some(layer) = leaf_layer.get(id) {
        *layer
    } else {
        let children = graph.sorted_children(id);
        children
            .iter()
            .map(|child| order_key(graph, child, leaf_layer, cache))
            .min()
            .unwrap_or(usize::MAX)
    };
    cache.insert(id.clone(), key);
    key
}

/// Build one unpositioned [`Item`] for `id`: a leaf (real drawn module, or
/// a node with no children at all) or a collapsed-namespace summary row
/// stop recursion; anything else is an expanded namespace, boxing its own
/// (recursively built, then shelf-packed) children.
fn build_item(
    graph: &ProjectGraph,
    id: &NodeId,
    collapsed: &HashSet<NodeId>,
    leaf_layer: &HashMap<NodeId, usize>,
    order_cache: &mut HashMap<NodeId, usize>,
    leaf_label: &impl Fn(&NodeId) -> String,
) -> Item {
    let is_leaf_like = collapsed.contains(id)
        || graph
            .node(id)
            .map(|node| node.children.is_empty())
            .unwrap_or(true);

    if is_leaf_like {
        let width = leaf_label(id).chars().count().max(1);
        return Item {
            id: id.clone(),
            rect: Rect {
                x: 0,
                y: 0,
                w: width,
                h: 1,
            },
            kind: ItemKind::Leaf,
        };
    }

    let mut children = graph.sorted_children(id);
    children.sort_by(|a, b| {
        order_key(graph, a, leaf_layer, order_cache).cmp(&order_key(
            graph,
            b,
            leaf_layer,
            order_cache,
        ))
    });

    let child_items: Vec<Item> = children
        .iter()
        .map(|child| build_item(graph, child, collapsed, leaf_layer, order_cache, leaf_label))
        .collect();
    let packed = shelf_pack(child_items);
    let (children_w, children_h) = bounding_size(&packed);

    let name = graph
        .node(id)
        .map(|n| n.display_name.clone())
        .unwrap_or_else(|| id.to_string());
    // Conservative minimum so the box is always at least wide enough for a
    // one-line title (`"\u{256d}\u{2500} name \u{2500}\u{256e}"`-shaped --
    // see `crate::tui::render`'s border-painting code for the exact glyphs);
    // the renderer is the one that actually draws the border text, this
    // just reserves the room for it.
    let title_min_w = name.chars().count() + 8;
    let box_w = (children_w + 2 * BOX_MARGIN).max(title_min_w);
    let box_h = children_h + 2 * BOX_MARGIN;

    // Shift every top-level child by the box's own border+padding margin --
    // from here on their rects are relative to this box's own (0, 0) origin.
    let offset_children: Vec<Item> = packed
        .into_iter()
        .map(|mut item| {
            item.rect.x += BOX_MARGIN;
            item.rect.y += BOX_MARGIN;
            item
        })
        .collect();

    Item {
        id: id.clone(),
        rect: Rect {
            x: 0,
            y: 0,
            w: box_w,
            h: box_h,
        },
        kind: ItemKind::Box(offset_children),
    }
}

/// The bounding size of an already-packed sibling list: the rightmost edge
/// and bottommost edge any item reaches, `(0, 0)` for an empty list.
fn bounding_size(items: &[Item]) -> (usize, usize) {
    let mut w = 0;
    let mut h = 0;
    for item in items {
        w = w.max(item.rect.x + item.rect.w);
        h = h.max(item.rect.y + item.rect.h);
    }
    (w, h)
}

/// Shelf-pack `items` (already in their desired left-to-right order) left to
/// right, wrapping onto a new shelf row once [`GAP_X`]-separated placement
/// would exceed the target width (see the module doc's width-bias formula).
/// Returns `items` with `rect.x`/`rect.y` filled in; each item's own
/// internal contents (a [`ItemKind::Box`]'s children) are untouched --
/// they're already relative to *this* item's own origin, a separate
/// coordinate frame [`flatten`] threads through later.
fn shelf_pack(mut items: Vec<Item>) -> Vec<Item> {
    if items.is_empty() {
        return items;
    }

    let widest = items.iter().map(|i| i.rect.w).max().unwrap_or(1);
    let total_area: f64 = items.iter().map(|i| (i.rect.w * i.rect.h) as f64).sum();
    let target_w = ((total_area.sqrt().ceil() as usize) * 5 / 2).max(widest);

    let mut cur_x = 0usize;
    let mut cur_y = 0usize;
    let mut row_max_h = 0usize;
    let mut row_has_item = false;

    for item in &mut items {
        let w = item.rect.w;
        let h = item.rect.h;
        if row_has_item && cur_x + GAP_X + w > target_w {
            cur_y += row_max_h + GAP_Y;
            cur_x = 0;
            row_max_h = 0;
        }
        item.rect.x = cur_x;
        item.rect.y = cur_y;
        cur_x += w + GAP_X;
        row_max_h = row_max_h.max(h);
        row_has_item = true;
    }

    items
}

/// Walk `item` and its descendants, adding `offset` (this item's parent's
/// absolute origin) to every rect and recording the result into `out` --
/// see the module doc's coordinate-frame note for why this addition is
/// always correct without any further scaling.
fn flatten(item: &Item, offset: (usize, usize), out: &mut PlaneLayout) {
    let abs = Rect {
        x: offset.0 + item.rect.x,
        y: offset.1 + item.rect.y,
        w: item.rect.w,
        h: item.rect.h,
    };
    out.width = out.width.max(abs.right());
    out.height = out.height.max(abs.bottom());
    match &item.kind {
        ItemKind::Leaf => {
            out.rows.insert(item.id.clone(), abs);
        }
        ItemKind::Box(children) => {
            out.boxes.insert(item.id.clone(), abs);
            out.children_of.insert(
                item.id.clone(),
                children.iter().map(|c| c.id.clone()).collect(),
            );
            for child in children {
                flatten(child, (abs.x, abs.y), out);
            }
        }
    }
}

/// `(layers, rows)` for [`crate::core::focus::move_focus`]: visible rows
/// grouped by their shared `y` (shelf rows -- and top-level placement --
/// always align siblings to one exact `y`, so this groups cleanly), each
/// group ordered by `x` ascending, groups themselves ordered by `y`
/// ascending. The same grouping feeds both `layers` (`h`/`l`, index-stepping
/// within a group) and `rows` (`j`/`k`, x-nearest in the adjacent group) --
/// mirrors [`crate::tui::render::canvas_focus_grid`]'s own precedent of
/// building both from one shared banding.
/// One `x`-ordered group of same-`y` visible rows, paired with each row's
/// `x`-center -- [`focus_grid`]'s `rows` output, the plane-view analog of
/// `crate::tui::render::CanvasFocusRows`.
pub type FocusRows = Vec<Vec<(NodeId, f32)>>;

pub fn focus_grid(layout: &PlaneLayout) -> (Vec<Vec<NodeId>>, FocusRows) {
    let mut by_y: Vec<(usize, NodeId, f32)> = layout
        .rows
        .iter()
        .map(|(id, rect)| (rect.y, id.clone(), rect.x_center()))
        .collect();
    by_y.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.2.partial_cmp(&b.2).unwrap_or(Ordering::Equal))
    });

    let mut layers: Vec<Vec<NodeId>> = Vec::new();
    let mut rows: Vec<Vec<(NodeId, f32)>> = Vec::new();
    let mut current_y: Option<usize> = None;
    for (y, id, x_center) in by_y {
        if current_y != Some(y) {
            layers.push(Vec::new());
            rows.push(Vec::new());
            current_y = Some(y);
        }
        layers.last_mut().expect("just pushed").push(id.clone());
        rows.last_mut().expect("just pushed").push((id, x_center));
    }
    (layers, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{FileRef, GitStatus, ModuleNode};
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
                status: GitStatus::Modified,
                files: vec![FileRef {
                    path: PathBuf::from(format!("{id}.rs")),
                    base_blob: None,
                    head_blob: None,
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

    fn label(id: &NodeId) -> String {
        // Deterministic and distinct per id, mirroring `crate::tui::render`'s
        // real "status glyph + name" convention closely enough for these
        // structural tests without dragging `core::App` in.
        format!("* {id}")
    }

    /// `ns` (namespace) containing `a`, `b`, plus a standalone root `c` --
    /// enough shape to exercise nesting, top-level packing, and multiple
    /// items in one container.
    fn nested_fixture() -> ProjectGraph {
        let (ns_id, ns) = namespace("ns", "Ns", None, &["a", "b"]);
        let (a_id, a) = leaf("a", "A", Some("ns"));
        let (b_id, b) = leaf("b", "B", Some("ns"));
        let (c_id, c) = leaf("c", "C", None);

        let mut nodes = HashMap::new();
        nodes.insert(ns_id.clone(), ns);
        nodes.insert(a_id, a);
        nodes.insert(b_id, b);
        nodes.insert(c_id.clone(), c);

        ProjectGraph {
            roots: vec![ns_id, c_id],
            nodes,
            edges: vec![],
        }
    }

    fn layers_fixture() -> Vec<Vec<NodeId>> {
        vec![
            vec![NodeId::from("a"), NodeId::from("c")],
            vec![NodeId::from("b")],
        ]
    }

    #[test]
    fn leaf_rows_have_height_one_and_width_matching_the_label() {
        let g = nested_fixture();
        let layout = layout(&g, &layers_fixture(), &HashSet::new(), label);
        let a_rect = layout.rows.get(&NodeId::from("a")).expect("a is a row");
        assert_eq!(a_rect.h, 1);
        assert_eq!(a_rect.w, label(&NodeId::from("a")).chars().count());
    }

    #[test]
    fn expanded_namespace_produces_a_box_strictly_containing_its_children() {
        let g = nested_fixture();
        let layout = layout(&g, &layers_fixture(), &HashSet::new(), label);
        let ns_box = layout.boxes.get(&NodeId::from("ns")).expect("ns is a box");
        let a_rect = layout.rows.get(&NodeId::from("a")).expect("a is a row");
        let b_rect = layout.rows.get(&NodeId::from("b")).expect("b is a row");
        assert!(ns_box.contains(a_rect));
        assert!(ns_box.contains(b_rect));
        // Strictly inside, not flush with the box's own border: at least one
        // cell of margin on every side.
        assert!(a_rect.x > ns_box.x);
        assert!(a_rect.y > ns_box.y);
        assert!(a_rect.right() < ns_box.right());
        assert!(a_rect.bottom() < ns_box.bottom());
    }

    #[test]
    fn collapsed_namespace_renders_as_a_single_row_not_a_box() {
        let g = nested_fixture();
        let collapsed = HashSet::from([NodeId::from("ns")]);
        let layout = layout(&g, &layers_fixture(), &collapsed, label);
        assert!(layout.rows.contains_key(&NodeId::from("ns")));
        assert!(!layout.boxes.contains_key(&NodeId::from("ns")));
        assert!(!layout.rows.contains_key(&NodeId::from("a")));
        assert!(!layout.rows.contains_key(&NodeId::from("b")));
    }

    #[test]
    fn no_two_top_level_rects_overlap() {
        let g = nested_fixture();
        let layout = layout(&g, &layers_fixture(), &HashSet::new(), label);
        let ns_box = layout.boxes.get(&NodeId::from("ns")).unwrap();
        let c_rect = layout.rows.get(&NodeId::from("c")).unwrap();
        assert!(!ns_box.overlaps(c_rect));
    }

    #[test]
    fn no_two_sibling_children_overlap() {
        let g = nested_fixture();
        let layout = layout(&g, &layers_fixture(), &HashSet::new(), label);
        let a_rect = layout.rows.get(&NodeId::from("a")).unwrap();
        let b_rect = layout.rows.get(&NodeId::from("b")).unwrap();
        assert!(!a_rect.overlaps(b_rect));
    }

    #[test]
    fn layout_is_deterministic_across_repeated_calls() {
        let g = nested_fixture();
        let layers = layers_fixture();
        let first = layout(&g, &layers, &HashSet::new(), label);
        let second = layout(&g, &layers, &HashSet::new(), label);
        assert_eq!(first, second);
    }

    #[test]
    fn children_are_ordered_by_layer_then_name() {
        // `a` sits in layer 0, `b` in layer 1 -- `a` must come first in
        // `ns`'s packed child order regardless of name (`a` < `b`
        // alphabetically too here, so also add a name-tiebreak case below).
        let g = nested_fixture();
        let layout = layout(&g, &layers_fixture(), &HashSet::new(), label);
        let children = layout.children_of.get(&NodeId::from("ns")).unwrap();
        assert_eq!(children, &vec![NodeId::from("a"), NodeId::from("b")]);
    }

    #[test]
    fn same_layer_siblings_tie_break_by_name() {
        let (ns_id, ns) = namespace("ns", "Ns", None, &["zeta", "alpha"]);
        let (zeta_id, zeta) = leaf("zeta", "Zeta", Some("ns"));
        let (alpha_id, alpha) = leaf("alpha", "Alpha", Some("ns"));
        let mut nodes = HashMap::new();
        nodes.insert(ns_id.clone(), ns);
        nodes.insert(zeta_id, zeta);
        nodes.insert(alpha_id, alpha);
        let g = ProjectGraph {
            roots: vec![ns_id.clone()],
            nodes,
            edges: vec![],
        };
        // Both in the same (only) layer -- name must decide the order.
        let layers = vec![vec![NodeId::from("zeta"), NodeId::from("alpha")]];
        let layout = layout(&g, &layers, &HashSet::new(), label);
        let children = layout.children_of.get(&ns_id).unwrap();
        assert_eq!(children, &vec![NodeId::from("alpha"), NodeId::from("zeta")]);
    }

    #[test]
    fn top_level_items_are_also_ordered_by_layer_then_name() {
        let g = nested_fixture();
        let layout = layout(&g, &layers_fixture(), &HashSet::new(), label);
        // `c` is layer 0, `ns`'s minimum descendant layer is `a`'s (layer 0
        // too) -- tie, so name decides: "C" < "Ns"? Compare by node id
        // ordering used through `order_key`/`sorted_roots`; assert the
        // top_level list is non-empty and contains both, and is stable
        // across repeated calls (covered by the determinism test) -- the
        // exact tie-break here is name via `sorted_roots`, verified by
        // checking `c` precedes `ns` (`"c" < "ns"` lexically, and `sort_by`
        // is stable so `sorted_roots`'s own name order survives the equal
        // layer-key comparison).
        assert_eq!(
            layout.top_level,
            vec![NodeId::from("c"), NodeId::from("ns")]
        );
    }

    #[test]
    fn shelf_pack_wraps_many_wide_children_onto_multiple_rows() {
        // A namespace with several equally-wide children and a narrow
        // target width (forced by their combined area) should wrap onto
        // more than one shelf row rather than a single very-wide row.
        let mut nodes = HashMap::new();
        let mut child_ids = Vec::new();
        for i in 0..12 {
            let cid = format!("leaf{i}");
            let (id, node) = leaf(&cid, "AVeryLongLabelName", None);
            nodes.insert(id.clone(), node);
            child_ids.push(cid);
        }
        let child_refs: Vec<&str> = child_ids.iter().map(String::as_str).collect();
        let (ns_id, ns) = namespace("ns", "Ns", None, &child_refs);
        nodes.insert(ns_id.clone(), ns);
        for cid in &child_ids {
            nodes.get_mut(&NodeId::from(cid.as_str())).unwrap().parent = Some(ns_id.clone());
        }
        let g = ProjectGraph {
            roots: vec![ns_id.clone()],
            nodes,
            edges: vec![],
        };
        let layout = layout(&g, &[], &HashSet::new(), label);
        let mut ys: Vec<usize> = child_ids
            .iter()
            .map(|c| layout.rows.get(&NodeId::from(c.as_str())).unwrap().y)
            .collect();
        ys.sort_unstable();
        ys.dedup();
        assert!(
            ys.len() > 1,
            "expected children wrapped onto multiple shelf rows"
        );
    }

    #[test]
    fn empty_graph_yields_an_empty_layout() {
        let g = ProjectGraph {
            roots: vec![],
            nodes: HashMap::new(),
            edges: vec![],
        };
        let layout = layout(&g, &[], &HashSet::new(), label);
        assert!(layout.rows.is_empty());
        assert!(layout.boxes.is_empty());
        assert_eq!(layout.width, 0);
        assert_eq!(layout.height, 0);
    }

    #[test]
    fn focus_grid_groups_same_y_rows_together_ordered_by_x() {
        let g = nested_fixture();
        let layout = layout(&g, &layers_fixture(), &HashSet::new(), label);
        let (layers, rows) = focus_grid(&layout);
        assert_eq!(layers.len(), rows.len());
        // `c` sits at the top level alongside `ns`'s own box, not inside any
        // group with `a`/`b` (which live inside `ns`'s box, at a deeper y).
        let c_group = layers
            .iter()
            .find(|group| group.contains(&NodeId::from("c")))
            .expect("c is in some group");
        assert!(!c_group.contains(&NodeId::from("a")));
    }
}
