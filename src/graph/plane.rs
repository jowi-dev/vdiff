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
    let mut visible_cache: HashMap<NodeId, bool> = HashMap::new();

    let mut roots = graph.sorted_roots();
    roots.retain(|id| is_visible(graph, id, collapsed, &mut visible_cache));
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
                &mut visible_cache,
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

/// Whether `id` should ever occupy a row/box in the plane layout at all:
/// currently collapsed (a real, visible summary row -- see
/// [`Msg::FocusSet`]'s own guard, which accepts exactly this same
/// condition), drawn (`node.files` non-empty -- present in `App::layers`,
/// so [`Msg::FocusSet`]'s guard accepts it too), or having at least one
/// visible descendant (recursively) -- an ordinary synthetic namespace
/// container, boxing real content below it. A node that's none of these --
/// a childless, file-less synthetic namespace with nothing left under it --
/// carries no id [`Msg::FocusSet`]'s guard would ever accept, so giving it a
/// row/box here would draw a cell no hjkl press could ever land on (see the
/// module doc's own note on why this matters). This shape is reachable in
/// practice: [`crate::graph::test_modules::hide_test_modules`] (and
/// [`crate::graph::filter::focus_on_changes`]'s own ancestor-hiding pass)
/// can prune every child out of a namespace whose own `files` were already
/// empty, leaving exactly this orphan behind. Memoized in `cache` for the
/// same reason [`order_key`]'s own cache exists -- the same id's visibility
/// can be asked about repeatedly across sibling comparisons and the parent
/// filtering pass below.
///
/// [`Msg::FocusSet`]: crate::core::app::Msg::FocusSet
fn is_visible(
    graph: &ProjectGraph,
    id: &NodeId,
    collapsed: &HashSet<NodeId>,
    cache: &mut HashMap<NodeId, bool>,
) -> bool {
    if collapsed.contains(id) {
        return true;
    }
    if let Some(visible) = cache.get(id) {
        return *visible;
    }
    let visible = match graph.node(id) {
        None => false,
        Some(node) if !node.files.is_empty() => true,
        Some(node) => node
            .children
            .iter()
            .any(|child| is_visible(graph, child, collapsed, cache)),
    };
    cache.insert(id.clone(), visible);
    visible
}

/// Build one unpositioned [`Item`] for `id`: a leaf (real drawn module, or
/// a node with no children at all) or a collapsed-namespace summary row
/// stop recursion; anything else is an expanded namespace, boxing its own
/// (recursively built, then shelf-packed) children. Only ever called on an
/// `id` [`is_visible`] already accepted -- see that function's doc for why
/// a node failing it must never reach here.
fn build_item(
    graph: &ProjectGraph,
    id: &NodeId,
    collapsed: &HashSet<NodeId>,
    leaf_layer: &HashMap<NodeId, usize>,
    order_cache: &mut HashMap<NodeId, usize>,
    visible_cache: &mut HashMap<NodeId, bool>,
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
    // Drop any child that would itself resolve to nothing focusable (see
    // [`is_visible`]'s doc) before recursing -- a namespace whose only
    // children are such orphans must not box them into existence.
    children.retain(|child| is_visible(graph, child, collapsed, visible_cache));
    children.sort_by(|a, b| {
        order_key(graph, a, leaf_layer, order_cache).cmp(&order_key(
            graph,
            b,
            leaf_layer,
            order_cache,
        ))
    });

    let mut child_items: Vec<Item> = children
        .iter()
        .map(|child| {
            build_item(
                graph,
                child,
                collapsed,
                leaf_layer,
                order_cache,
                visible_cache,
                leaf_label,
            )
        })
        .collect();
    // A *drawn* namespace (a real module with its own backing file that also
    // has children -- `crate::graph::builder`'s "real defmodule takes
    // precedence over synthetic namespace" shape) keeps a focusable self-row
    // as its box's first child: without one, its own diff is unreachable
    // from this view and any edge terminating at the namespace itself
    // silently vanishes ([`crate::graph::plane_edges`] skips endpoints
    // missing from [`PlaneLayout::rows`]). Same id lands in both
    // [`PlaneLayout::rows`] (the self-row) and [`PlaneLayout::boxes`] (the
    // container) -- the two maps are keyed independently, and the renderer
    // paints borders and labels in separate passes.
    let is_drawn = graph
        .node(id)
        .map(|node| !node.files.is_empty())
        .unwrap_or(false);
    if is_drawn {
        let width = leaf_label(id).chars().count().max(1);
        child_items.insert(
            0,
            Item {
                id: id.clone(),
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: width,
                    h: 1,
                },
                kind: ItemKind::Leaf,
            },
        );
    }
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
    use std::collections::VecDeque;
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

    /// A *drawn* namespace -- a real module with its own backing file that
    /// also has children (`crate::graph::builder`'s "real defmodule takes
    /// precedence over synthetic namespace" shape, e.g. `defmodule AppWeb`
    /// with `AppWeb.Foo` submodules). When expanded, it must render as a box
    /// AND keep a focusable self-row inside that box -- without one, its own
    /// diff can't be opened from the plane view at all and any edge whose
    /// endpoint is the namespace itself silently vanishes
    /// (`crate::graph::plane_edges::route_one` skips endpoints missing from
    /// [`PlaneLayout::rows`]).
    #[test]
    fn a_drawn_namespace_keeps_a_self_row_inside_its_own_box() {
        let (ns_id, mut ns) = namespace("ns", "Ns", None, &["a", "b"]);
        ns.files = vec![FileRef {
            path: PathBuf::from("ns.rs"),
            base_blob: None,
            head_blob: None,
        }];
        let (a_id, a) = leaf("a", "A", Some("ns"));
        let (b_id, b) = leaf("b", "B", Some("ns"));
        let mut nodes = HashMap::new();
        nodes.insert(ns_id.clone(), ns);
        nodes.insert(a_id, a);
        nodes.insert(b_id, b);
        let g = ProjectGraph {
            roots: vec![ns_id.clone()],
            nodes,
            edges: vec![],
        };

        let layout = layout(&g, &layers_fixture(), &HashSet::new(), label);

        let box_rect = *layout.boxes.get(&ns_id).expect("ns renders as a box");
        let self_rect = *layout
            .rows
            .get(&ns_id)
            .expect("drawn namespace keeps a focusable self-row");
        assert!(
            box_rect.contains(&self_rect),
            "self-row {self_rect:?} must sit inside the box {box_rect:?}"
        );
        for (other, rect) in &layout.rows {
            if other != &ns_id {
                assert!(!rect.overlaps(&self_rect), "self-row overlaps {other}");
            }
        }
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

    /// Reproduces the shape [`crate::graph::test_modules::hide_test_modules`]
    /// (and [`crate::graph::filter::focus_on_changes`]'s ancestor-hiding
    /// pass) can leave behind: a synthetic namespace (`files` empty, per
    /// [`namespace`]'s own construction) whose every child got pruned away,
    /// leaving `children` empty too. Such a node is neither drawn
    /// (`App::is_drawn`, which only ever consults `App::layers` --
    /// file-backed nodes -- says no) nor collapsed, so
    /// `Msg::FocusSet`'s guard permanently rejects it as a target; before
    /// the fix, [`build_item`]'s `is_leaf_like` check (`children.is_empty()`
    /// alone, with no `files` check) drew it as an ordinary leaf row anyway
    /// -- present on screen and in [`PlaneLayout::rows`], but unreachable by
    /// any hjkl press, and (per [`focus_grid`]'s single global y-ordering)
    /// splitting every row below it off from every row above it, since `j`/
    /// `k` only ever step to the *adjacent* y-group and this row's group
    /// could never become focus to make that step from.
    #[test]
    fn orphan_namespace_with_no_files_and_no_children_gets_no_row() {
        let (root_id, root) = namespace("root", "Root", None, &["left", "orphan", "right"]);
        let (left_id, left) = leaf("left", "Left", Some("root"));
        let (orphan_id, orphan) = namespace("orphan", "Orphan", Some("root"), &[]);
        let (right_id, right) = leaf("right", "Right", Some("root"));

        let mut nodes = HashMap::new();
        nodes.insert(root_id.clone(), root);
        nodes.insert(left_id.clone(), left);
        nodes.insert(orphan_id.clone(), orphan);
        nodes.insert(right_id.clone(), right);
        let g = ProjectGraph {
            roots: vec![root_id.clone()],
            nodes,
            edges: vec![],
        };

        let layout = layout(&g, &[], &HashSet::new(), label);

        assert!(
            !layout.rows.contains_key(&orphan_id),
            "a childless, file-less orphan must not get a focusable row"
        );
        assert!(!layout.boxes.contains_key(&orphan_id));
        let root_children = layout.children_of.get(&root_id).expect("root is a box");
        assert!(
            !root_children.contains(&orphan_id),
            "the orphan must not be listed as one of root's children either"
        );
        assert!(layout.rows.contains_key(&left_id));
        assert!(layout.rows.contains_key(&right_id));
    }

    /// A collapsed namespace has no children *in this layout* either (it
    /// renders as a single summary row -- see
    /// [`collapsed_namespace_renders_as_a_single_row_not_a_box`]), but it
    /// must still get a row: [`Msg::FocusSet`]'s guard accepts any
    /// currently-collapsed id regardless of `files`, so [`is_visible`] has
    /// to check `collapsed` before falling back to the files/children test.
    #[test]
    fn a_collapsed_namespace_with_no_files_still_gets_a_row() {
        let (root_id, root) = namespace("root", "Root", None, &["ns"]);
        let (ns_id, ns) = namespace("ns", "Ns", Some("root"), &["a"]);
        let (a_id, a) = leaf("a", "A", Some("ns"));
        let mut nodes = HashMap::new();
        nodes.insert(root_id.clone(), root);
        nodes.insert(ns_id.clone(), ns);
        nodes.insert(a_id, a);
        let g = ProjectGraph {
            roots: vec![root_id],
            nodes,
            edges: vec![],
        };
        let collapsed = HashSet::from([ns_id.clone()]);
        let layout = layout(&g, &[], &collapsed, label);
        assert!(layout.rows.contains_key(&ns_id));
    }

    /// Reproduces a mismatch [`is_visible`]'s issue-#21 fix does NOT cover:
    /// `crate::tui::render::build_plane_view` calls this module's [`layout`]
    /// with `app.graph` (the full, *un-test-pruned* graph -- see
    /// `App::graph`'s own doc, "never changes -- it's always the full,
    /// focus-filtered graph") but with `app.layers`, which is instead
    /// derived from `App::visible_graph()` -- `hide_test_modules`'s pruned
    /// graph when `show_tests` is off (the default), or
    /// `group_matched_test_modules`'s pruned graph when it's on (see
    /// `crate::core::app::toggle_tests`). Both prune functions delegate to
    /// [`crate::graph::filter::prune`], which deletes the matched test node
    /// from the graph's `nodes` map outright -- so a matched test module is
    /// simply absent from `app.layers` (and never in `App::fold_collapsed`
    /// either). But [`is_visible`]/[`build_item`] only ever consult the
    /// `graph` argument's own `files`/`children` -- a *separate*, unpruned
    /// copy in this call shape -- so a test module with a backing file still
    /// reads as visible and gets an ordinary leaf row here. That row's id
    /// fails `Msg::FocusSet`'s guard (`App::is_drawn`, which only consults
    /// `app.layers`) exactly the way issue #21's orphan-namespace row did,
    /// walling off every row past it in [`focus_grid`]'s single global
    /// y-ordering -- but `is_visible`'s fix, which only ever reasons about
    /// one `graph` argument, cannot see this: the bug isn't in what `layout`
    /// computes from its inputs, it's that its two structural inputs
    /// (`graph`, `layers`) come from two different prune passes over the
    /// same underlying graph at the real call site. This test models that
    /// exact two-graph split without needing `crate::core::app::App` at
    /// all: `layers_fixture_from` (test-pruned) stands in for `app.layers`,
    /// `full` (unpruned) stands in for `app.graph`.
    #[test]
    fn test_module_pruned_from_layers_but_not_from_graph_gets_an_unfocusable_row() {
        // `ns` (a drawn namespace with its own file, e.g. `BidConnectors`)
        // contains `real` (e.g. `DynamicBids`) and `real_test` (e.g.
        // `DynamicBidsCustomPipelineTest` -- a matched/hidden test module:
        // present with a backing file in the *full* graph, but that's the
        // one piece of information `App::visible_graph()`'s test-hiding
        // pass strips before `app.layers` is computed from it).
        let (ns_id, mut ns) = namespace("ns", "BidConnectors", None, &["real", "real_test"]);
        ns.files = vec![FileRef {
            path: PathBuf::from("ns.rs"),
            base_blob: None,
            head_blob: None,
        }];
        let (real_id, real) = leaf("real", "DynamicBids", Some("ns"));
        let (test_id, test_node) = leaf("real_test", "DynamicBidsCustomPipelineTest", Some("ns"));

        let mut nodes = HashMap::new();
        nodes.insert(ns_id.clone(), ns);
        nodes.insert(real_id.clone(), real);
        nodes.insert(test_id.clone(), test_node);
        let full_graph = ProjectGraph {
            roots: vec![ns_id.clone()],
            nodes,
            edges: vec![],
        };

        // What `App::layers` would actually hold: `assign_layers` run over
        // `hide_test_modules`'s pruned graph, i.e. `real_test` never
        // appears in any layer (mirrors `App::is_drawn`'s own predicate).
        let (test_pruned_graph, hidden_count) =
            crate::graph::test_modules::hide_test_modules(&full_graph);
        assert_eq!(hidden_count, 1, "the fixture's one test module was pruned");
        let layers_like_app = crate::graph::layers::assign_layers(&test_pruned_graph);
        let is_drawn_per_app_layers =
            |id: &NodeId| -> bool { layers_like_app.iter().any(|layer| layer.contains(id)) };
        assert!(
            !is_drawn_per_app_layers(&test_id),
            "sanity: the test module must be absent from the app.layers stand-in"
        );

        // The real call shape: `layout` gets the *full*, unpruned graph
        // (standing in for `app.graph`) paired with the test-pruned layers
        // (standing in for `app.layers`) -- exactly `build_plane_view`'s
        // `plane::layout(&app.graph, &app.layers, ...)` call.
        let layout = layout(&full_graph, &layers_like_app, &HashSet::new(), label);

        assert!(
            layout.rows.contains_key(&test_id),
            "the test module still gets an ordinary plane row from the unpruned graph"
        );
        assert!(
            !is_drawn_per_app_layers(&test_id),
            "...but Msg::FocusSet's guard (App::is_drawn via app.layers) would reject that same id"
        );
        // This is precisely the unfocusable-row shape: a row `layout`
        // happily emits that `Msg::FocusSet` can never actually land focus
        // on, because its two inputs disagree about whether `real_test`
        // exists at all.
    }

    /// Mirrors `Msg::FocusSet`'s own guard (`App::is_drawn(&id) ||
    /// App::fold_collapsed.contains(&id)`, see `crate::core::app`) without
    /// needing a whole `App`: `App::is_drawn` only ever consults
    /// `App::layers`, and [`crate::graph::layers::drawn_node_ids`] (what
    /// populates it) is exactly "has at least one file" -- independent of
    /// children, so this is the same predicate restated over `graph`
    /// directly. A row [`layout`] emits that this rejects is a row hjkl can
    /// approach but never actually focus -- issue #21's bug, and precisely
    /// what [`is_visible`] now keeps out of the layout in the first place.
    fn focus_set_would_accept(
        graph: &ProjectGraph,
        id: &NodeId,
        collapsed: &HashSet<NodeId>,
    ) -> bool {
        collapsed.contains(id)
            || graph
                .node(id)
                .map(|node| !node.files.is_empty())
                .unwrap_or(false)
    }

    /// BFS over [`crate::core::focus::move_focus`] (the same function
    /// `crate::tui::plane_key_msg` dispatches h/j/k/l through) from every
    /// possible starting row must reach every other row in
    /// [`PlaneLayout::rows`], and every row it reaches must be one
    /// `Msg::FocusSet` would actually accept (see
    /// [`focus_set_would_accept`]) -- issue #21's bug wasn't a hole in
    /// [`move_focus`]'s own candidate selection (that function's grouped-
    /// by-y/x-nearest shape is provably fully connected over whatever rows
    /// [`focus_grid`] hands it), it was [`layout`] emitting a row
    /// `Msg::FocusSet` could never land on, which turns that row's y-group
    /// into a wall: `j`/`k` only ever step to the *adjacent* group, so a
    /// group focus can never enter permanently splits every row past it off
    /// from every row before it. Exercised over several representative
    /// shapes: a plain nested namespace, a dense many-sibling shelf-wrap, a
    /// multi-level-nesting drawn-namespace tree, and (the shape that
    /// actually reproduces the issue) [`orphan_bearing_fixture`]'s pair of
    /// childless, file-less namespaces sitting between real rows.
    #[test]
    fn every_row_is_reachable_and_focusable_via_hjkl() {
        use crate::core::focus::{move_focus, Direction};

        let fixtures: Vec<ProjectGraph> = vec![
            nested_fixture(),
            wide_shelf_fixture(),
            multi_level_nesting_fixture(),
            orphan_bearing_fixture(),
        ];

        for g in fixtures {
            let collapsed = HashSet::new();
            let layout = layout(&g, &[], &collapsed, label);
            let (layers, rows) = focus_grid(&layout);
            let all_ids: HashSet<NodeId> = layout.rows.keys().cloned().collect();
            assert!(!all_ids.is_empty(), "fixture must have at least one row");
            for id in &all_ids {
                assert!(
                    focus_set_would_accept(&g, id, &collapsed),
                    "{id} has a plane row but FocusSet would reject it as a target"
                );
            }

            for start in &all_ids {
                let mut visited: HashSet<NodeId> = HashSet::from([start.clone()]);
                let mut queue: VecDeque<NodeId> = VecDeque::from([start.clone()]);
                while let Some(cur) = queue.pop_front() {
                    for dir in [
                        Direction::Left,
                        Direction::Right,
                        Direction::Up,
                        Direction::Down,
                    ] {
                        let next = move_focus(&layers, &rows, &cur, dir);
                        if visited.insert(next.clone()) {
                            queue.push_back(next);
                        }
                    }
                }
                let unreached: Vec<&NodeId> =
                    all_ids.iter().filter(|id| !visited.contains(*id)).collect();
                assert!(
                    unreached.is_empty(),
                    "starting from {start}, hjkl never reaches {unreached:?}"
                );
            }
        }
    }

    /// A namespace with several equally-wide siblings, forcing
    /// [`shelf_pack`] to wrap them onto more than one shelf row -- the
    /// dense-startup-fold-adjacent shape the connectivity property test
    /// needs (multiple rows genuinely stacked, not everything on one shelf).
    fn wide_shelf_fixture() -> ProjectGraph {
        let mut nodes = HashMap::new();
        let mut child_ids = Vec::new();
        for i in 0..10 {
            let cid = format!("wleaf{i}");
            let (id, node) = leaf(&cid, "AVeryLongLabelNameIndeed", None);
            nodes.insert(id.clone(), node);
            child_ids.push(cid);
        }
        let child_refs: Vec<&str> = child_ids.iter().map(String::as_str).collect();
        let (ns_id, ns) = namespace("wns", "Wns", None, &child_refs);
        nodes.insert(ns_id.clone(), ns);
        for cid in &child_ids {
            nodes.get_mut(&NodeId::from(cid.as_str())).unwrap().parent = Some(ns_id.clone());
        }
        ProjectGraph {
            roots: vec![ns_id],
            nodes,
            edges: vec![],
        }
    }

    /// Three levels of nesting (`top` > `mid` > `leaf`/`deep_leaf`), each
    /// level itself a *drawn* namespace (keeps a self-row), plus a top-level
    /// sibling -- exercises box-boundary candidate selection across more
    /// than one nesting depth at once.
    fn multi_level_nesting_fixture() -> ProjectGraph {
        let (top_id, mut top) = namespace("top", "Top", None, &["mid", "sibling"]);
        top.files = vec![FileRef {
            path: PathBuf::from("top.rs"),
            base_blob: None,
            head_blob: None,
        }];
        let (mid_id, mut mid) = namespace("mid", "Mid", Some("top"), &["leaf", "deep"]);
        mid.files = vec![FileRef {
            path: PathBuf::from("mid.rs"),
            base_blob: None,
            head_blob: None,
        }];
        let (leaf_id, leaf_node) = leaf("leaf", "Leaf", Some("mid"));
        let (deep_id, deep_node) = leaf("deep", "Deep", Some("mid"));
        let (sibling_id, sibling_node) = leaf("sibling", "Sibling", Some("top"));

        let mut nodes = HashMap::new();
        nodes.insert(top_id.clone(), top);
        nodes.insert(mid_id, mid);
        nodes.insert(leaf_id, leaf_node);
        nodes.insert(deep_id, deep_node);
        nodes.insert(sibling_id, sibling_node);
        ProjectGraph {
            roots: vec![top_id],
            nodes,
            edges: vec![],
        }
    }

    /// A wider version of the shape [`orphan_namespace_with_no_files_and_no_children_gets_no_row`]
    /// targets, sized for the connectivity sweep: two orphan namespaces
    /// (one a top-level root, one nested two levels deep) sitting between
    /// real drawn rows on every side, so a regression that turns either
    /// back into a row would strand every row past it in the global
    /// y-ordering (see [`focus_grid`]'s doc), not just fail a presence
    /// check.
    fn orphan_bearing_fixture() -> ProjectGraph {
        let (before_id, before) = leaf("before", "Before", None);
        let (root_orphan_id, root_orphan) = namespace("root_orphan", "RootOrphan", None, &[]);
        let (mid_id, mid) = namespace("mid2", "Mid2", None, &["inner_orphan", "after_mid"]);
        let (inner_orphan_id, inner_orphan) =
            namespace("inner_orphan", "InnerOrphan", Some("mid2"), &[]);
        let (after_mid_id, after_mid) = leaf("after_mid", "AfterMid", Some("mid2"));
        let (after_id, after) = leaf("after", "After", None);

        let mut nodes = HashMap::new();
        nodes.insert(before_id.clone(), before);
        nodes.insert(root_orphan_id.clone(), root_orphan);
        nodes.insert(mid_id.clone(), mid);
        nodes.insert(inner_orphan_id, inner_orphan);
        nodes.insert(after_mid_id, after_mid);
        nodes.insert(after_id.clone(), after);
        ProjectGraph {
            roots: vec![before_id, root_orphan_id, mid_id, after_id],
            nodes,
            edges: vec![],
        }
    }
}
