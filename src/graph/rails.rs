//! Pure, toolkit-neutral rail-gutter layout for the `--tui` graph screen's
//! git-log-style vertical DAG (issue #16 phase 2). This module knows nothing
//! about `ratatui`/terminals/colors -- it turns an ordered list of row ids
//! plus a set of edges between them into a per-row grid of gutter cells,
//! each a plain `char` (a box-drawing glyph) tagged with a [`RailRole`] the
//! caller (`crate::tui::render`) maps onto a terminal style. Kept in
//! `crate::graph` rather than `crate::tui` per the crate's core-purity rule:
//! no IO, no `ratatui`/`crossterm` types outside `crate::tui`.
//!
//! # The algorithm
//!
//! Every edge occupies exactly one rail *column* for the span of rows it
//! covers, keyed by the *unordered* row interval `(top_row, bottom_row)` =
//! `(min(from_row, to_row), max(from_row, to_row))` -- not by which
//! endpoint is the dependency's `from`/`to`. This matters once the caller
//! (`crate::core::rail_view::collapse_edges`) has folded a namespace: a
//! collapsed row absorbs descendants from several original layers, so a
//! perfectly ordinary dependency edge between two still-*visible* rows can
//! end up pointing visually upward in row order (the collapsed row lands
//! above a row that, structurally, depends on something inside it). Row
//! order here is a *display* order, not a guaranteed topological one, so
//! this module never assumes `from_row < to_row` for layout purposes --
//! only a same-row edge (both endpoints collapsed into the very same row)
//! has nothing left to rail and is dropped (see [`resolve_spans`]).
//! [`RailRole`], by contrast, *does* care about true dependency direction
//! (`FocusedOutgoing`/`FocusedIncoming` need to know which end is really
//! the focused node's dependency vs. dependent) -- see [`Span`]'s own doc
//! for how the two are kept independent.
//!
//! Columns are assigned greedily, like a classic interval-graph coloring:
//! process edges sorted by `(top_row, bottom_row)`, and for each one reuse
//! the lowest-numbered column whose current edge has already finished (its
//! `bottom_row` is `<=` this edge's `top_row`) -- otherwise open a new
//! column. This is exactly the minimum-track scheduling used by every
//! git-log-style renderer, and it guarantees the hard invariant this
//! module is unit-tested against: two edges whose row spans actually
//! overlap never share a column. Edges that merely *touch* (one's
//! `bottom_row` equals another's `top_row`) are free to share a column,
//! since only one of them is ever "in flight" at any single row.
//!
//! Each column-row cell is rendered from the one edge active there (if
//! any): `╮` at `top_row` (branching out of that row's own node), `╯` at
//! `bottom_row` (arriving at that row's own node), `│` for every row
//! strictly between the two. A column with no edge covering a given row
//! renders as a blank cell. This glyph choice is purely about the visual
//! span, independent of dependency direction -- an upward edge renders
//! with exactly the same `╮`/`│`/`╯` shape a downward one would over the
//! same two rows; only its [`RailRole`] color (if it touches focus) gives
//! away that it's semantically reversed.
//!
//! # The width cap
//!
//! [`compute`] takes the caller's terminal width and, if the full edge set
//! would need more rail columns than roughly a third of it, drops every
//! edge that doesn't touch `focus` and recomputes columns over just those --
//! see [`RailLayout::dropped_edges`], which the caller renders as a `+N
//! edges` hint in the legend strip instead of an unbounded gutter (see the
//! top-level task brief this module was built against for the rationale: a
//! change set's visible graph is usually 15-40 nodes, but a hairball of
//! cross-cutting dependency edges among them can still need far more rail
//! columns than a terminal is wide).

use std::collections::HashMap;

use crate::graph::model::NodeId;

/// How a rail cell's glyph relates to the currently focused row: dimmed by
/// default so a dense gutter stays readable, with the focused node's own
/// edges picked out in one of two accent colors -- see the module doc and
/// the `--tui` task brief's "two-accent convention" (mirrored from the GUI's
/// `crate::ui::theme::edge_stroke_outgoing`/`edge_stroke_incoming`, which
/// this module can't import directly since it must stay `gui`-feature-free).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailRole {
    /// Neither endpoint of this cell's edge is the focused row.
    Normal,
    /// This edge's `from_row` is the focused row -- the focused node's own
    /// outgoing dependency.
    FocusedOutgoing,
    /// This edge's `to_row` is the focused row -- the focused node's own
    /// dependent.
    FocusedIncoming,
}

/// One gutter cell: a box-drawing glyph plus which rail column it sits in
/// (columns are sparse -- most rows leave most columns blank, so cells are
/// stored as a list of occupied columns per row rather than a dense grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailCell {
    pub column: usize,
    pub glyph: char,
    pub role: RailRole,
}

/// [`compute`]'s output: one row of (possibly empty) [`RailCell`]s per input
/// row, the total column count actually used (so the caller can size the
/// gutter), and how many edges the width cap ([`compute`]'s doc) dropped --
/// `0` when every edge made it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailLayout {
    pub rows: Vec<Vec<RailCell>>,
    pub columns: usize,
    pub dropped_edges: usize,
}

/// Compute the rail gutter for `row_ids` (top-to-bottom display order) and
/// `edges` (pairs of ids that must both appear in `row_ids`; any edge whose
/// endpoints aren't both present, or that resolve to the same row, is
/// silently dropped -- see [`resolve_spans`]'s doc; an edge that resolves to
/// two *different* rows is always kept regardless of which direction it
/// points in that order -- see the module doc). `focus` picks out
/// [`RailRole::FocusedOutgoing`]/[`RailRole::FocusedIncoming`] cells.
/// `terminal_width` drives the cap described in the module doc; pass e.g.
/// the real terminal column count.
pub fn compute(
    row_ids: &[NodeId],
    edges: &[(NodeId, NodeId)],
    focus: &NodeId,
    terminal_width: usize,
) -> RailLayout {
    let row_of: HashMap<&NodeId, usize> =
        row_ids.iter().enumerate().map(|(i, id)| (id, i)).collect();

    let spans = resolve_spans(edges, &row_of);
    let full = layout_spans(&spans, row_ids.len(), focus);

    let cap = terminal_width / 3;
    if full.columns <= cap {
        return full;
    }

    let focused_spans: Vec<Span> = spans
        .iter()
        .filter(|s| &s.from == focus || &s.to == focus)
        .cloned()
        .collect();
    let dropped = spans.len() - focused_spans.len();
    let mut capped = layout_spans(&focused_spans, row_ids.len(), focus);
    capped.dropped_edges = dropped;
    capped
}

/// One edge resolved to row indices, keeping both the *visual* span
/// (`top_row`/`bottom_row`, unordered -- what column assignment and glyph
/// placement key off, see the module doc) and the *true* dependency
/// direction (`from`/`to`, the original edge's own endpoints -- what
/// [`RailRole`] keys off in [`layout_spans`]). These two are deliberately
/// independent: `top_row`/`bottom_row` can disagree with `from`'s/`to`'s
/// row order whenever the display order isn't a strict topological one
/// (see the module doc's fold-collapse example), but `from`/`to` always
/// still name the actual dependency, so `FocusedOutgoing`/`FocusedIncoming`
/// stay semantically correct regardless of which way the rail visually
/// points.
#[derive(Debug, Clone)]
struct Span {
    from: NodeId,
    to: NodeId,
    top_row: usize,
    bottom_row: usize,
}

/// Resolve `edges` to [`Span`]s, dropping any edge with an unknown endpoint
/// or whose two endpoints resolve to the *same* row -- both endpoints
/// collapsed into one visible row, so there's nothing left to rail (the
/// caller, `crate::core::rail_view::collapse_edges`, already drops these
/// itself before calling `compute`, but this stays defensive rather than
/// assuming). An edge whose endpoints resolve to two *different* rows is
/// always kept, however those rows happen to be ordered -- see the module
/// doc for why an "upward" edge is a real, expected case once fold-collapse
/// is in play, not a malformed input to guard against.
fn resolve_spans(edges: &[(NodeId, NodeId)], row_of: &HashMap<&NodeId, usize>) -> Vec<Span> {
    edges
        .iter()
        .filter_map(|(from, to)| {
            let from_row = *row_of.get(from)?;
            let to_row = *row_of.get(to)?;
            if from_row == to_row {
                return None;
            }
            let (top_row, bottom_row) = if from_row <= to_row {
                (from_row, to_row)
            } else {
                (to_row, from_row)
            };
            Some(Span {
                from: from.clone(),
                to: to.clone(),
                top_row,
                bottom_row,
            })
        })
        .collect()
}

/// A `Span` plus which column it was greedily assigned.
struct Assigned {
    span_idx: usize,
    column: usize,
}

/// Greedily assign each span a column (see the module doc's algorithm
/// section), then render every row's cells from the assignment, tagging
/// each cell [`RailRole::FocusedOutgoing`]/[`RailRole::FocusedIncoming`] if
/// its span touches `focus`, [`RailRole::Normal`] otherwise. Column
/// assignment itself never looks at `focus` -- it's purely a row-range
/// scheduling problem -- so this only affects which role each already-
/// placed cell reports.
fn layout_spans(spans: &[Span], row_count: usize, focus: &NodeId) -> RailLayout {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| (spans[i].top_row, spans[i].bottom_row));

    let mut busy_until: Vec<usize> = Vec::new();
    let mut assigned: Vec<Assigned> = Vec::with_capacity(spans.len());

    for span_idx in order {
        let span = &spans[span_idx];
        let free_column = busy_until
            .iter()
            .position(|&busy| busy <= span.top_row)
            .unwrap_or_else(|| {
                busy_until.push(0);
                busy_until.len() - 1
            });
        busy_until[free_column] = span.bottom_row;
        assigned.push(Assigned {
            span_idx,
            column: free_column,
        });
    }

    let mut rows: Vec<Vec<RailCell>> = vec![Vec::new(); row_count];
    for a in &assigned {
        let span = &spans[a.span_idx];
        // Role keys off the *true* dependency direction (`from`/`to`), not
        // the visual `top_row`/`bottom_row` span -- see `Span`'s own doc.
        let role = if &span.from == focus {
            RailRole::FocusedOutgoing
        } else if &span.to == focus {
            RailRole::FocusedIncoming
        } else {
            RailRole::Normal
        };
        for (row, cells) in rows
            .iter_mut()
            .enumerate()
            .take(span.bottom_row + 1)
            .skip(span.top_row)
        {
            let glyph = if row == span.top_row {
                '╮'
            } else if row == span.bottom_row {
                '╯'
            } else {
                '│'
            };
            cells.push(RailCell {
                column: a.column,
                glyph,
                role,
            });
        }
    }
    for row in &mut rows {
        row.sort_by_key(|c| c.column);
    }

    RailLayout {
        rows,
        columns: busy_until.len(),
        dropped_edges: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<NodeId> {
        names.iter().map(|n| NodeId::from(*n)).collect()
    }

    fn edge(from: &str, to: &str) -> (NodeId, NodeId) {
        (NodeId::from(from), NodeId::from(to))
    }

    fn cell_at(rows: &[Vec<RailCell>], row: usize, column: usize) -> Option<&RailCell> {
        rows[row].iter().find(|c| c.column == column)
    }

    #[test]
    fn straight_edge_spanning_one_row_gets_start_and_end_glyphs() {
        let rows = ids(&["a", "b"]);
        let edges = vec![edge("a", "b")];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        assert_eq!(layout.columns, 1);
        assert_eq!(cell_at(&layout.rows, 0, 0).unwrap().glyph, '╮');
        assert_eq!(cell_at(&layout.rows, 1, 0).unwrap().glyph, '╯');
    }

    #[test]
    fn edge_spanning_several_rows_has_vertical_passthrough_in_between() {
        let rows = ids(&["a", "b", "c", "d"]);
        let edges = vec![edge("a", "d")];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        assert_eq!(cell_at(&layout.rows, 0, 0).unwrap().glyph, '╮');
        assert_eq!(cell_at(&layout.rows, 1, 0).unwrap().glyph, '│');
        assert_eq!(cell_at(&layout.rows, 2, 0).unwrap().glyph, '│');
        assert_eq!(cell_at(&layout.rows, 3, 0).unwrap().glyph, '╯');
    }

    #[test]
    fn multiple_parallel_edges_get_distinct_columns() {
        // a->d and b->c overlap in row-range ([0,3] vs [1,2]) so they must
        // land in different columns.
        let rows = ids(&["a", "b", "c", "d"]);
        let edges = vec![edge("a", "d"), edge("b", "c")];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        assert_eq!(layout.columns, 2);
        let row1_columns: Vec<usize> = layout.rows[1].iter().map(|c| c.column).collect();
        assert_eq!(row1_columns.len(), 2, "both edges active at row 1");
    }

    #[test]
    fn edges_sharing_a_start_row_fan_out_to_distinct_columns() {
        let rows = ids(&["a", "b", "c"]);
        let edges = vec![edge("a", "b"), edge("a", "c")];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        assert_eq!(layout.columns, 2, "both edges leave row 0 at once");
        assert_eq!(layout.rows[0].len(), 2);
    }

    #[test]
    fn edges_sharing_an_end_row_fan_in_to_distinct_columns() {
        let rows = ids(&["a", "b", "c"]);
        let edges = vec![edge("a", "c"), edge("b", "c")];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        assert_eq!(layout.columns, 2, "both edges arrive at row 2 at once");
        assert_eq!(layout.rows[2].len(), 2);
    }

    #[test]
    fn touching_edges_reuse_the_same_column() {
        // a->b ends exactly where b->c starts (both at row 1): no row-range
        // overlap, so the greedy scheduler should reuse one column for both.
        let rows = ids(&["a", "b", "c"]);
        let edges = vec![edge("a", "b"), edge("b", "c")];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        assert_eq!(layout.columns, 1, "touching edges share a column");
    }

    #[test]
    fn no_overlapping_distinct_edges_ever_share_a_column() {
        // A denser fixture: several edges with genuinely overlapping spans.
        // Whatever column assignment comes out, no two spans assigned to the
        // same column may have overlapping row ranges.
        let rows = ids(&["a", "b", "c", "d", "e"]);
        let edges = vec![
            edge("a", "d"),
            edge("a", "c"),
            edge("b", "e"),
            edge("b", "d"),
        ];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        // Reconstruct each column's occupied row set from the rendered
        // cells and check no column has two "starts" without an
        // intervening "end" implying overlap -- concretely: for every row,
        // a column must have at most one cell (already true by
        // construction, but this is the behavioral invariant the test
        // exists to pin down).
        for row in &layout.rows {
            let mut seen = std::collections::HashSet::new();
            for cell in row {
                assert!(
                    seen.insert(cell.column),
                    "column {} appears twice in one row",
                    cell.column
                );
            }
        }
    }

    #[test]
    fn edge_with_unknown_endpoint_is_dropped_without_panicking() {
        let rows = ids(&["a", "b"]);
        let edges = vec![edge("a", "ghost")];
        let layout = compute(&rows, &edges, &NodeId::from("a"), 200);
        assert_eq!(layout.columns, 0);
    }

    #[test]
    fn same_row_edges_are_dropped() {
        let rows = ids(&["a", "b"]);
        let edges = vec![edge("a", "a")];
        let layout = compute(&rows, &edges, &NodeId::from("a"), 200);
        assert_eq!(layout.columns, 0);
    }

    #[test]
    fn an_upward_edge_between_visible_rows_still_renders_a_rail() {
        // `edge("b", "a")` is "upward" in row order (b sits below a) --
        // e.g. what a fold-collapsed row can produce (see the module doc).
        // It must still get a column and the same `╮`/`╯` glyphs a
        // downward edge over the same two rows would.
        let rows = ids(&["a", "b"]);
        let edges = vec![edge("b", "a")];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        assert_eq!(layout.columns, 1, "upward edge must still occupy a column");
        assert_eq!(cell_at(&layout.rows, 0, 0).unwrap().glyph, '╮');
        assert_eq!(cell_at(&layout.rows, 1, 0).unwrap().glyph, '╯');
    }

    #[test]
    fn an_upward_edge_spanning_several_rows_still_gets_passthrough_cells() {
        let rows = ids(&["a", "b", "c", "d"]);
        let edges = vec![edge("d", "a")];
        let layout = compute(&rows, &edges, &NodeId::from("nobody"), 200);

        assert_eq!(cell_at(&layout.rows, 0, 0).unwrap().glyph, '╮');
        assert_eq!(cell_at(&layout.rows, 1, 0).unwrap().glyph, '│');
        assert_eq!(cell_at(&layout.rows, 2, 0).unwrap().glyph, '│');
        assert_eq!(cell_at(&layout.rows, 3, 0).unwrap().glyph, '╯');
    }

    #[test]
    fn an_upward_edges_role_reflects_true_dependency_direction_when_focused() {
        // `b` (row 1, visually below `a`) is the true dependency source:
        // `edge("b", "a")` means "b depends on a". Focusing `b` must color
        // the rail as `b`'s own *outgoing* dependency, even though visually
        // the rail's `top_row` (0) is `a`, not `b`.
        let rows = ids(&["a", "b"]);
        let edges = vec![edge("b", "a")];

        let layout = compute(&rows, &edges, &NodeId::from("b"), 200);
        let cell = cell_at(&layout.rows, 0, 0).unwrap();
        assert_eq!(cell.role, RailRole::FocusedOutgoing);

        let layout = compute(&rows, &edges, &NodeId::from("a"), 200);
        let cell = cell_at(&layout.rows, 0, 0).unwrap();
        assert_eq!(cell.role, RailRole::FocusedIncoming);
    }

    #[test]
    fn downward_edge_behavior_is_unchanged_by_the_unordered_span_fix() {
        // Sanity check against regressions: a plain downward edge behaves
        // exactly as before -- straight span, correct roles both ways.
        let rows = ids(&["a", "b"]);
        let edges = vec![edge("a", "b")];

        let layout = compute(&rows, &edges, &NodeId::from("a"), 200);
        assert_eq!(
            cell_at(&layout.rows, 0, 0).unwrap().role,
            RailRole::FocusedOutgoing
        );

        let layout = compute(&rows, &edges, &NodeId::from("b"), 200);
        assert_eq!(
            cell_at(&layout.rows, 1, 0).unwrap().role,
            RailRole::FocusedIncoming
        );
    }

    #[test]
    fn wide_gutter_degrades_to_focus_only_rails_with_a_dropped_count() {
        // Ten fully-overlapping edges need ten columns -- comfortably over
        // a 9-column cap (terminal_width 27 / 3 = 9). None of them touch
        // the focus node, so after the cap kicks in the gutter should be
        // empty and every edge counted as dropped.
        let rows_ids: Vec<String> = (0..12).map(|i| format!("n{i}")).collect();
        let rows: Vec<NodeId> = rows_ids.iter().map(|s| NodeId::from(s.as_str())).collect();
        let edges: Vec<(NodeId, NodeId)> = (0..10)
            .map(|i| (NodeId::from(format!("n{i}")), NodeId::from("n11")))
            .collect();

        let layout = compute(&rows, &edges, &NodeId::from("n0"), 27);

        assert!(layout.columns <= 9, "capped gutter must respect the cap");
        // n0's own edge (n0 -> n11) touches focus, so it must survive.
        assert!(layout.dropped_edges > 0, "some edges must be dropped");
        assert_eq!(layout.dropped_edges, 9, "9 of 10 edges don't touch focus");
    }
}
