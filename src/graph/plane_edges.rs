//! Pure edge routing for the `--tui` plane graph screen, over
//! [`crate::graph::plane::PlaneLayout`]'s absolute char space. Deliberately
//! the simplest routing that reads correctly for a nested layout, mirroring
//! the GUI's own straight under-box dependency lines (see
//! `crate::graph::layout`/`crate::ui::graph_view`): a 3-segment orthogonal
//! route -- vertical out of the source label, horizontal at a midpoint row,
//! vertical into the target label -- with **no obstacle avoidance**. That's
//! a deliberate v1 simplification (see the module's own "what this doesn't
//! do" note below), not an oversight.
//!
//! # Why garbling is impossible by construction
//!
//! This module only ever produces cells; it never decides how they're
//! composited against box borders/labels. `crate::tui::render`'s plane
//! renderer paints these cells *first*, then paints every box/label *over*
//! them (see that module's doc) -- so an edge cell that happens to fall
//! inside a box or under a label is simply overwritten, never the reverse.
//! An edge is only ever visible where it passes through genuinely empty
//! space, which -- thanks to [`crate::graph::plane`]'s shelf-packing gaps
//! (2 blank columns between siblings, 1 blank row between shelf rows) --
//! is most of the space an orthogonal route actually needs.
//!
//! # What this doesn't do
//!
//! No obstacle avoidance (a route can cross through where an unrelated box
//! would be, though the renderer's paint-order guarantees that box still
//! reads correctly -- only the edge segment underneath it is invisible); no
//! bend-row scheduling like [`crate::graph::canvas`]'s per-channel budget
//! (each edge picks its own midpoint row independently, so two routed edges
//! can and do overlap when their paths happen to coincide -- resolved the
//! same way [`crate::graph::canvas::merge_glyph`] already resolves a real
//! `│`/`─` crossing into `┼`, reused here directly).

use std::collections::HashMap;

use crate::graph::canvas::{merge_glyph, CanvasCell, CanvasRole};
use crate::graph::model::NodeId;
use crate::graph::plane::PlaneLayout;

/// Above this many visible edges, only edges touching the focused node or
/// one of its direct neighbors are actually routed -- mirrors
/// [`crate::graph::rails::compute`]'s `dropped_edges`/
/// [`crate::graph::canvas::Channel::dropped`] conventions for the same
/// "don't drown the screen" problem, just at the whole-view level instead
/// of per-rail/per-channel (a nested layout has no single channel to budget
/// against).
pub const EDGE_BUDGET: usize = 80;

/// One routed cell in the plane's own absolute char space (as opposed to
/// [`CanvasCell`]'s column-only, per-channel-relative space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneEdgeCell {
    pub x: usize,
    pub y: usize,
    pub glyph: char,
    pub role: CanvasRole,
}

/// [`route_edges`]'s output: the routed cells (deterministically sorted by
/// `(y, x)`), and how many edges the [`EDGE_BUDGET`] degrade dropped
/// entirely (not stubbed, unlike the canvas view's channel budget -- a
/// plane edge that's dropped just isn't drawn at all; the legend hint is
/// `crate::tui::render`'s job, mirroring `Channel::dropped`'s own "+N edges"
/// convention).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlaneEdges {
    pub cells: Vec<PlaneEdgeCell>,
    pub hidden: usize,
}

/// Route every edge in `edges` (as returned by
/// [`crate::core::rail_view::collapse_edges`]) over `layout`'s rects,
/// tagging cells [`CanvasRole::FocusedOutgoing`]/[`CanvasRole::FocusedIncoming`]
/// relative to `focus` exactly like [`crate::graph::canvas::route_channels`]
/// does. Draw order: every [`CanvasRole::Normal`] edge first, then every
/// focused edge -- so a focused edge always wins a cell conflict (see
/// [`write_segment`]'s merge behavior) -- with real `┼` crossings where two
/// segments genuinely cross.
pub fn route_edges(layout: &PlaneLayout, edges: &[(NodeId, NodeId)], focus: &NodeId) -> PlaneEdges {
    let visible: Vec<&(NodeId, NodeId)> = if edges.len() > EDGE_BUDGET {
        let neighbors = neighbor_set(edges, focus);
        edges
            .iter()
            .filter(|(from, to)| neighbors.contains(from) || neighbors.contains(to))
            .collect()
    } else {
        edges.iter().collect()
    };
    let hidden = edges.len() - visible.len();

    let mut normal = Vec::new();
    let mut focused = Vec::new();
    for edge in visible {
        let role = role_of(edge, focus);
        match role {
            CanvasRole::Normal => normal.push((edge, role)),
            _ => focused.push((edge, role)),
        }
    }

    let mut grid: HashMap<(usize, usize), (char, CanvasRole)> = HashMap::new();
    for (edge, role) in normal.into_iter().chain(focused) {
        route_one(layout, &edge.0, &edge.1, role, &mut grid);
    }

    let mut cells: Vec<PlaneEdgeCell> = grid
        .into_iter()
        .map(|((x, y), (glyph, role))| PlaneEdgeCell { x, y, glyph, role })
        .collect();
    cells.sort_by_key(|c| (c.y, c.x));

    PlaneEdges { cells, hidden }
}

fn role_of(edge: &(NodeId, NodeId), focus: &NodeId) -> CanvasRole {
    if &edge.0 == focus {
        CanvasRole::FocusedOutgoing
    } else if &edge.1 == focus {
        CanvasRole::FocusedIncoming
    } else {
        CanvasRole::Normal
    }
}

/// `focus` plus every node directly connected to it by an edge (either
/// direction) -- the "focused node and its direct neighbors" [`route_edges`]'s
/// budget degrade keeps at full fidelity.
fn neighbor_set(edges: &[(NodeId, NodeId)], focus: &NodeId) -> std::collections::HashSet<NodeId> {
    let mut set = std::collections::HashSet::new();
    set.insert(focus.clone());
    for (from, to) in edges {
        if from == focus {
            set.insert(to.clone());
        }
        if to == focus {
            set.insert(from.clone());
        }
    }
    set
}

/// Route one edge's 3-segment orthogonal path: exit `from`'s label at its
/// x-center (bottom edge if `to` sits below, top edge otherwise), a vertical
/// run to a midpoint row strictly between the two labels, a horizontal run
/// at that row, then a vertical run into `to`'s x-center (top or bottom
/// edge, whichever faces `from`). A same-column pair (`from`'s x-center
/// rounds to the same column as `to`'s) skips the horizontal run and corner
/// glyphs entirely -- just one continuous `│`. Missing rects (an edge
/// endpoint not present in `layout.rows` at all -- shouldn't happen for an
/// edge [`crate::core::rail_view::collapse_edges`] produced from the same
/// fold state this layout was built from, but this stays defensive) are
/// silently skipped rather than panicking.
fn route_one(
    layout: &PlaneLayout,
    from: &NodeId,
    to: &NodeId,
    role: CanvasRole,
    grid: &mut HashMap<(usize, usize), (char, CanvasRole)>,
) {
    let Some(src) = layout.rows.get(from) else {
        return;
    };
    let Some(tgt) = layout.rows.get(to) else {
        return;
    };
    if from == to {
        return;
    }

    let x_src = src.x_center().round() as usize;
    let x_tgt = tgt.x_center().round() as usize;

    let target_below = tgt.y >= src.y + src.h;
    let (y_start, y_end) = if target_below {
        (src.y + src.h, tgt.y.saturating_sub(1))
    } else {
        (src.y.saturating_sub(1), tgt.y + tgt.h)
    };
    let (lo, hi) = (y_start.min(y_end), y_start.max(y_end));
    let mid = lo + (hi - lo) / 2;

    let mut write = |x: usize, y: usize, glyph: char| {
        grid.entry((x, y))
            .and_modify(|(existing_glyph, existing_role)| {
                *existing_glyph = merge_glyph(*existing_glyph, glyph);
                if *existing_role == CanvasRole::Normal {
                    *existing_role = role;
                }
            })
            .or_insert((glyph, role));
    };

    if x_src == x_tgt {
        for y in lo..=hi {
            write(x_src, y, '│');
        }
        return;
    }

    // Vertical leg out of the source, up to (and including) the bend row.
    let (top, bottom) = (y_start.min(mid), y_start.max(mid));
    for y in top..=bottom {
        write(x_src, y, '│');
    }
    // Horizontal leg at the bend row.
    let (left, right) = (x_src.min(x_tgt), x_src.max(x_tgt));
    for x in left..=right {
        write(x, mid, '─');
    }
    write(x_src, mid, '╮');
    write(x_tgt, mid, '╯');
    // Vertical leg into the target, strictly past the bend row -- the bend
    // row's own cell already holds the `╯` corner just written above, and
    // must not be reclaimed by this leg (a naive inclusive range here used
    // to let this loop run last and clobber that corner back into a bare
    // `│`).
    if mid < y_end {
        for y in (mid + 1)..=y_end {
            write(x_tgt, y, '│');
        }
    } else if mid > y_end {
        for y in y_end..mid {
            write(x_tgt, y, '│');
        }
    }
}

/// Cast [`PlaneEdgeCell`]s into the plane renderer's canvas-cell vocabulary
/// where convenient (kept as a distinct free function rather than a `From`
/// impl since the coordinate meaning genuinely differs -- see the module
/// doc -- and an implicit conversion would blur that).
pub fn as_canvas_cell(cell: &PlaneEdgeCell) -> CanvasCell {
    CanvasCell {
        column: cell.x,
        glyph: cell.glyph,
        role: cell.role,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::plane::Rect;

    fn id(name: &str) -> NodeId {
        NodeId::from(name)
    }

    fn layout_with(rows: &[(&str, usize, usize, usize, usize)]) -> PlaneLayout {
        let mut out = PlaneLayout::default();
        for (name, x, y, w, h) in rows {
            out.rows.insert(
                id(name),
                Rect {
                    x: *x,
                    y: *y,
                    w: *w,
                    h: *h,
                },
            );
        }
        out
    }

    #[test]
    fn same_column_edge_is_a_straight_vertical_run() {
        let layout = layout_with(&[("a", 5, 0, 3, 1), ("b", 5, 5, 3, 1)]);
        let edges = vec![(id("a"), id("b"))];
        let routed = route_edges(&layout, &edges, &id("nobody"));
        assert!(routed.cells.iter().all(|c| c.glyph == '│'));
        assert!(!routed.cells.is_empty());
    }

    #[test]
    fn bent_edge_gets_both_corner_glyphs() {
        let layout = layout_with(&[("a", 0, 0, 3, 1), ("b", 20, 5, 3, 1)]);
        let edges = vec![(id("a"), id("b"))];
        let routed = route_edges(&layout, &edges, &id("nobody"));
        assert!(routed.cells.iter().any(|c| c.glyph == '╮'));
        assert!(routed.cells.iter().any(|c| c.glyph == '╯'));
    }

    #[test]
    fn upward_edge_routes_from_target_below_to_source_above() {
        // `b` sits above `a` in y, but the edge still goes from `a` to `b`
        // -- exercise the `target_below == false` branch.
        let layout = layout_with(&[("a", 0, 10, 3, 1), ("b", 20, 0, 3, 1)]);
        let edges = vec![(id("a"), id("b"))];
        let routed = route_edges(&layout, &edges, &id("nobody"));
        assert!(!routed.cells.is_empty());
    }

    #[test]
    fn focused_edge_is_tagged_and_drawn_last() {
        let layout = layout_with(&[("a", 0, 0, 3, 1), ("b", 0, 5, 3, 1)]);
        let edges = vec![(id("a"), id("b"))];
        let routed = route_edges(&layout, &edges, &id("a"));
        assert!(routed
            .cells
            .iter()
            .all(|c| c.role == CanvasRole::FocusedOutgoing));
    }

    #[test]
    fn crossing_segments_render_a_plus_glyph() {
        // A straight vertical at x=5 the whole height, plus a bent edge
        // whose horizontal run crosses x=5 at its own bend row.
        let layout = layout_with(&[
            ("s_top", 5, 0, 1, 1),
            ("s_bottom", 5, 10, 1, 1),
            ("b_top", 0, 4, 1, 1),
            ("b_bottom", 10, 4, 1, 1),
        ]);
        let edges = vec![(id("s_top"), id("s_bottom")), (id("b_top"), id("b_bottom"))];
        let routed = route_edges(&layout, &edges, &id("nobody"));
        assert!(routed.cells.iter().any(|c| c.glyph == '┼'));
    }

    #[test]
    fn missing_endpoint_is_skipped_without_panicking() {
        let layout = layout_with(&[("a", 0, 0, 3, 1)]);
        let edges = vec![(id("a"), id("ghost"))];
        let routed = route_edges(&layout, &edges, &id("a"));
        assert!(routed.cells.is_empty());
    }

    #[test]
    fn self_edge_is_skipped() {
        let layout = layout_with(&[("a", 0, 0, 3, 1)]);
        let edges = vec![(id("a"), id("a"))];
        let routed = route_edges(&layout, &edges, &id("a"));
        assert!(routed.cells.is_empty());
    }

    #[test]
    fn over_budget_edge_set_keeps_only_focus_and_direct_neighbors() {
        let mut rows: Vec<(&str, usize, usize, usize, usize)> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for i in 0..(EDGE_BUDGET + 10) {
            names.push(format!("n{i}"));
        }
        for (i, name) in names.iter().enumerate() {
            rows.push((name.as_str(), i * 4, 0, 3, 1));
        }
        rows.push(("focus", 0, 5, 3, 1));
        rows.push(("far", 400, 5, 3, 1));
        let layout = layout_with(&rows);

        let mut edges: Vec<(NodeId, NodeId)> = Vec::new();
        // One real edge touching focus.
        edges.push((id("focus"), id(&names[0])));
        // A pile of unrelated edges to push the total over budget.
        for i in 1..names.len() {
            edges.push((id(&names[i - 1]), id(&names[i])));
        }
        assert!(edges.len() > EDGE_BUDGET);

        let routed = route_edges(&layout, &edges, &id("focus"));
        assert!(routed.hidden > 0);
        assert!(routed.cells.iter().any(|c| c.role != CanvasRole::Normal));
    }

    #[test]
    fn under_budget_edge_set_hides_nothing() {
        let layout = layout_with(&[("a", 0, 0, 3, 1), ("b", 0, 5, 3, 1)]);
        let edges = vec![(id("a"), id("b"))];
        let routed = route_edges(&layout, &edges, &id("a"));
        assert_eq!(routed.hidden, 0);
    }

    #[test]
    fn as_canvas_cell_preserves_glyph_and_role() {
        let cell = PlaneEdgeCell {
            x: 3,
            y: 4,
            glyph: '│',
            role: CanvasRole::FocusedIncoming,
        };
        let canvas_cell = as_canvas_cell(&cell);
        assert_eq!(canvas_cell.column, 3);
        assert_eq!(canvas_cell.glyph, '│');
        assert_eq!(canvas_cell.role, CanvasRole::FocusedIncoming);
    }

    #[test]
    fn cells_are_sorted_by_row_then_column() {
        let layout = layout_with(&[("a", 0, 0, 3, 1), ("b", 20, 10, 3, 1)]);
        let edges = vec![(id("a"), id("b"))];
        let routed = route_edges(&layout, &edges, &id("nobody"));
        let mut sorted = routed.cells.clone();
        sorted.sort_by_key(|c| (c.y, c.x));
        assert_eq!(routed.cells, sorted);
    }
}
