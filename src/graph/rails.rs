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
//! Every edge `(from_row, to_row)` (`from_row < to_row` -- the caller is
//! expected to have already dropped/collapsed edges that don't run strictly
//! downward, since the row order is the graph's topological layer order)
//! occupies exactly one rail *column* for the span of rows it covers.
//! Columns are assigned greedily, like a classic interval-graph coloring:
//! process edges sorted by `(from_row, to_row)`, and for each one reuse the
//! lowest-numbered column whose current edge has already finished (its
//! `to_row` is `<=` this edge's `from_row`) -- otherwise open a new column.
//! This is exactly the minimum-track scheduling used by every git-log-style
//! renderer, and it guarantees the hard invariant this module is unit-tested
//! against: two edges whose row spans actually overlap never share a column.
//! Edges that merely *touch* (one's `to_row` equals another's `from_row`)
//! are free to share a column, since only one of them is ever "in flight" at
//! any single row.
//!
//! Each column-row cell is rendered from the one edge active there (if any):
//! `╮` where the edge starts (`row == from_row`, branching down out of the
//! row's own node), `╯` where it ends (`row == to_row`, arriving back at the
//! row's own node), `│` for every row strictly between the two. A column
//! with no edge covering a given row renders as a blank cell.
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
/// endpoints aren't both present, or that isn't strictly downward in row
/// order, is silently dropped -- the caller is responsible for handing in a
/// graph-consistent edge set, but this stays total rather than panicking on
/// a malformed one). `focus` picks out [`RailRole::FocusedOutgoing`]/
/// [`RailRole::FocusedIncoming`] cells. `terminal_width` drives the cap
/// described in the module doc; pass e.g. the real terminal column count.
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

/// One edge resolved to row indices plus the original ids (kept so
/// [`compute`]'s focus filter can still check identity after resolution).
#[derive(Debug, Clone)]
struct Span {
    from: NodeId,
    to: NodeId,
    from_row: usize,
    to_row: usize,
}

/// Resolve `edges` to [`Span`]s, dropping any edge with an unknown endpoint
/// or that isn't strictly downward (`from_row < to_row`) -- a same-row edge
/// has nothing to rail (the caller should have already deduped/dropped
/// those before calling `compute`), and a reversed one shouldn't occur given
/// a proper topological row order but is dropped defensively rather than
/// panicking.
fn resolve_spans(edges: &[(NodeId, NodeId)], row_of: &HashMap<&NodeId, usize>) -> Vec<Span> {
    edges
        .iter()
        .filter_map(|(from, to)| {
            let from_row = *row_of.get(from)?;
            let to_row = *row_of.get(to)?;
            if from_row >= to_row {
                return None;
            }
            Some(Span {
                from: from.clone(),
                to: to.clone(),
                from_row,
                to_row,
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
    order.sort_by_key(|&i| (spans[i].from_row, spans[i].to_row));

    let mut busy_until: Vec<usize> = Vec::new();
    let mut assigned: Vec<Assigned> = Vec::with_capacity(spans.len());

    for span_idx in order {
        let span = &spans[span_idx];
        let free_column = busy_until
            .iter()
            .position(|&busy| busy <= span.from_row)
            .unwrap_or_else(|| {
                busy_until.push(0);
                busy_until.len() - 1
            });
        busy_until[free_column] = span.to_row;
        assigned.push(Assigned {
            span_idx,
            column: free_column,
        });
    }

    let mut rows: Vec<Vec<RailCell>> = vec![Vec::new(); row_count];
    for a in &assigned {
        let span = &spans[a.span_idx];
        let role = if &span.from == focus {
            RailRole::FocusedOutgoing
        } else if &span.to == focus {
            RailRole::FocusedIncoming
        } else {
            RailRole::Normal
        };
        for row in span.from_row..=span.to_row {
            let glyph = if row == span.from_row {
                '╮'
            } else if row == span.to_row {
                '╯'
            } else {
                '│'
            };
            rows[row].push(RailCell {
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

    fn cell_at<'a>(rows: &'a [Vec<RailCell>], row: usize, column: usize) -> Option<&'a RailCell> {
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
    fn reversed_or_same_row_edges_are_dropped() {
        let rows = ids(&["a", "b"]);
        let edges = vec![edge("b", "a"), edge("a", "a")];
        let layout = compute(&rows, &edges, &NodeId::from("a"), 200);
        assert_eq!(layout.columns, 0);
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
