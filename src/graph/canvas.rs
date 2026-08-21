//! Pure, toolkit-neutral orthogonal edge routing for the `--tui` canvas
//! graph screen's inter-band channels (issue #17). Companion to
//! [`crate::graph::sugiyama`], which does the band/x-coordinate layout and
//! the dummy-node chaining that turns every edge into a sequence of
//! adjacent-band hops -- this module's whole job is turning one such hop
//! (a `(from_x, to_x)` pair between band `i` and band `i+1`) into an actual
//! row of box-drawing glyphs, the char-cell channel that sits between two
//! rendered bands. `crate::tui::render` draws the bands' own labels
//! directly from [`crate::graph::sugiyama::Layout::bands`] (status colors,
//! badges -- display glue this module has no business owning, mirroring
//! how [`crate::graph::rails`] computes only the gutter and leaves
//! [`crate::tui::render::node_line`] to compose the label); this module is
//! the canvas's equivalent of that gutter, just horizontal instead of
//! vertical.
//!
//! # Per-channel routing
//!
//! Every edge that touches this channel is either **straight**
//! (`from_x == to_x`: draw `│` straight down at that column for the whole
//! channel height) or **bent** (needs a horizontal jog). Bent segments are
//! greedily assigned a "bend row" within the channel via the same
//! interval-scheduling idea [`crate::graph::rails::compute`] uses for its
//! rail columns, just transposed onto x-ranges instead of row-ranges: sort
//! by `(min_x, max_x)`, reuse the lowest bend row whose already-assigned
//! segment's x-range doesn't overlap this one's, otherwise open a new bend
//! row. A bent segment then renders as `│` from the channel's top down to
//! its bend row at `from_x`, a `─`-filled run from `min_x` to `max_x` at
//! the bend row itself (`╮` at `from_x`, `╯` at `to_x` -- see the module
//! doc's note on why these two glyphs mark "departure"/"arrival" rather
//! than true left/right turn geometry, matching
//! [`crate::graph::rails`]'s own precedent for the same box-drawing
//! constraint), then `│` from the bend row down to the channel's bottom at
//! `to_x`. Wherever two different segments' cells collide (a straight
//! segment's vertical passing through a bent segment's horizontal run, or
//! vice versa), the crossing renders as `┼` rather than either original
//! glyph silently overwriting the other.
//!
//! # Channel height budget (issue #18)
//!
//! Even with issue #18's median coordinate assignment straightening most
//! edges (see [`crate::graph::sugiyama`]'s doc), a real change set can still
//! pile more bent edges into one channel than [`CHANNEL_HEIGHT_BUDGET`] rows
//! -- without a cap, that used to mean a dozen-plus rows of routing
//! spaghetti (see the issue's own real-use screenshot). Once a channel's
//! *non-focused* bent edges would need more than the budget's worth of bend
//! rows, the excess ones degrade: no bend row, no `╮`/`╯`, just a bare `╷`
//! at the channel's very top row (departing the upper band) and `╵` at its
//! very bottom row (arriving at the lower band), and [`Channel::dropped`]
//! counts them so `crate::tui::render` can surface a "+N edges not drawn"
//! legend hint. The currently focused node's own edges are exempt from the
//! cap entirely -- they always get a real bend row, however many that
//! takes -- because "the focused node's edges are followable end to end"
//! is the one thing this screen must never sacrifice (see the issue's own
//! acceptance criteria). A degraded edge's stub is only ever painted into
//! an otherwise-empty cell (see [`route_one_channel`]'s doc on write
//! ordering), so it can never obscure a real bend, straight line, or the
//! focused node's own full-fidelity edge -- worst case, in an especially
//! dense channel, a stub simply doesn't render at all rather than
//! clobbering something more important.
//!
//! # What this deliberately doesn't handle
//!
//! Band-wrap (splitting an overflowing band into multiple node rows within
//! itself) is `crate::tui::render`'s concern, not this module's -- it needs
//! the actual terminal width, which neither this module nor
//! [`crate::graph::sugiyama`] takes at all. A wrapped band's internal
//! sub-row transitions render with no rail continuity at all (a documented
//! known limitation -- see the `--tui` canvas screen's module doc), since
//! this module only ever routes *between* two whole bands, never within
//! one.

use std::collections::HashMap;

use crate::graph::model::NodeId;
use crate::graph::sugiyama::Layout;

/// How a channel cell's glyph relates to the currently focused node --
/// identical in spirit to [`crate::graph::rails::RailRole`], just for the
/// canvas's horizontal channels instead of the rail view's vertical ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasRole {
    Normal,
    FocusedOutgoing,
    FocusedIncoming,
}

/// One routed cell: a box-drawing glyph at a given column, tagged with
/// which rail it belongs to for coloring. Cells are stored sparsely (one
/// list of occupied columns per row), mirroring
/// [`crate::graph::rails::RailCell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasCell {
    pub column: usize,
    pub glyph: char,
    pub role: CanvasRole,
}

/// The channel height budget (issue #18's fix 2): once a channel needs more
/// than this many bend rows for its *non-focused* edges, additional normal
/// edges degrade to endpoint stubs (see [`route_one_channel`]) rather than
/// growing the channel further -- a hairball of a dozen overlapping bent
/// edges used to mean a dozen rows of routing spaghetti; capping it here is
/// what actually makes "channels a few rows tall" (the issue's acceptance
/// criterion) true regardless of how tangled the underlying graph is. The
/// focused node's own edges are exempt (see [`Channel::dropped`]'s doc) --
/// they can still push the channel taller than this when they need to.
pub const CHANNEL_HEIGHT_BUDGET: usize = 5;

/// One inter-band channel's routed cells: `rows[r]` is row `r`'s (sparse)
/// cell list, `height` rows total. `dropped` is how many *non-focused*
/// edges in this channel exceeded [`CHANNEL_HEIGHT_BUDGET`] and were
/// degraded to endpoint stubs (a bare `╷`/`╵` at the channel's top/bottom
/// row instead of a full bend) rather than growing the channel further --
/// `crate::tui::render` sums this across every channel to surface a dim
/// "+N edges not drawn" legend note, mirroring
/// [`crate::graph::rails::compute`]'s own `dropped_edges` convention for
/// the rail view's width cap.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Channel {
    pub height: usize,
    pub rows: Vec<Vec<CanvasCell>>,
    pub dropped: usize,
}

/// Route every inter-band channel in `layout`: one [`Channel`] per gap
/// between consecutive bands (`layout.bands.len().saturating_sub(1)` of
/// them, in top-to-bottom order), `focus` picking out
/// [`CanvasRole::FocusedOutgoing`]/[`CanvasRole::FocusedIncoming`] cells
/// exactly like [`crate::graph::rails::compute`] does for the rail view.
pub fn route_channels(layout: &Layout, focus: &NodeId) -> Vec<Channel> {
    (0..layout.bands.len().saturating_sub(1))
        .map(|channel| route_one_channel(layout, channel, focus))
        .collect()
}

/// One edge's hop across a single channel, plus which role it should
/// render in.
struct Segment {
    from_x: usize,
    to_x: usize,
    role: CanvasRole,
}

fn role_of(edge: &crate::graph::sugiyama::RoutedEdge, focus: &NodeId) -> CanvasRole {
    if &edge.from == focus {
        CanvasRole::FocusedOutgoing
    } else if &edge.to == focus {
        CanvasRole::FocusedIncoming
    } else {
        CanvasRole::Normal
    }
}

fn route_one_channel(layout: &Layout, channel: usize, focus: &NodeId) -> Channel {
    let mut segments = Vec::new();
    for edge in &layout.edges {
        for pair in edge.waypoints.windows(2) {
            let (band_a, x_a) = pair[0];
            let (band_b, x_b) = pair[1];
            if band_a == channel && band_b == channel + 1 {
                segments.push(Segment {
                    from_x: x_a.round() as usize,
                    to_x: x_b.round() as usize,
                    role: role_of(edge, focus),
                });
            }
        }
    }

    let (straight, bent): (Vec<&Segment>, Vec<&Segment>) =
        segments.iter().partition(|s| s.from_x == s.to_x);

    // Greedy interval scheduling of bent segments onto bend rows, keyed by
    // x-range overlap -- the transposed twin of `rails::layout_spans`'s
    // row-range scheduling. A focused segment (`role != Normal`) always
    // gets a fresh row when no existing one is free, exactly like the
    // pre-budget code; a normal segment only gets a fresh row while the
    // channel is still under `CHANNEL_HEIGHT_BUDGET` -- past that, it has
    // no row assigned at all (`bend_row_of[idx] == None`), which
    // [`Self`]'s caller below renders as a degraded endpoint stub instead
    // of a real bend (see [`Channel::dropped`]'s doc).
    let mut order: Vec<usize> = (0..bent.len()).collect();
    order.sort_by_key(|&i| {
        let s = bent[i];
        (s.from_x.min(s.to_x), s.from_x.max(s.to_x))
    });
    let mut busy_until: Vec<usize> = Vec::new();
    let mut bend_row_of: Vec<Option<usize>> = vec![None; bent.len()];
    let mut dropped = 0usize;
    for idx in order {
        let s = bent[idx];
        let (lo, hi) = (s.from_x.min(s.to_x), s.from_x.max(s.to_x));
        let free_row = busy_until.iter().position(|&busy| busy <= lo);
        let row = match free_row {
            Some(row) => Some(row),
            None if s.role != CanvasRole::Normal || busy_until.len() < CHANNEL_HEIGHT_BUDGET => {
                busy_until.push(0);
                Some(busy_until.len() - 1)
            }
            None => None,
        };
        match row {
            Some(row) => {
                busy_until[row] = hi;
                bend_row_of[idx] = Some(row);
            }
            None => dropped += 1,
        }
    }

    let height = busy_until.len().max(1);
    let mut grid: HashMap<(usize, usize), (char, CanvasRole)> = HashMap::new();
    let mut write = |row: usize, col: usize, glyph: char, role: CanvasRole| {
        grid.entry((row, col))
            .and_modify(|(existing_glyph, existing_role)| {
                *existing_glyph = merge_glyph(*existing_glyph, glyph);
                if *existing_role == CanvasRole::Normal {
                    *existing_role = role;
                }
            })
            .or_insert((glyph, role));
    };

    for s in &straight {
        for row in 0..height {
            write(row, s.from_x, '│', s.role);
        }
    }
    // Full-fidelity bends (straight lines above and any focused/in-budget
    // bent segment) are drawn before any degraded stub below, and never
    // afterward -- see the loop below's own comment for why that order
    // matters: a stub must never be able to clobber a real bend/line, only
    // ever fill in a genuinely empty cell.
    for (idx, s) in bent.iter().enumerate() {
        if let Some(bend_row) = bend_row_of[idx] {
            for row in 0..bend_row {
                write(row, s.from_x, '│', s.role);
            }
            let (lo, hi) = (s.from_x.min(s.to_x), s.from_x.max(s.to_x));
            for col in lo..=hi {
                write(bend_row, col, '─', s.role);
            }
            write(bend_row, s.from_x, '╮', s.role);
            write(bend_row, s.to_x, '╯', s.role);
            for row in (bend_row + 1)..height {
                write(row, s.to_x, '│', s.role);
            }
        }
    }
    for (idx, s) in bent.iter().enumerate() {
        if bend_row_of[idx].is_none() {
            // Degraded: the channel is already at budget, so this normal
            // edge gets no bend row at all, just a bare departure/arrival
            // marker at the channel's top and bottom rows -- enough to see
            // the edge exists without paying for its own row. Drawn last,
            // and only into a cell nothing else has claimed yet (`or_insert`
            // with no `and_modify`), so a degraded edge can never overwrite
            // -- or, by drawing last, be overwritten by -- a real bend, a
            // straight line, or the focused node's own full-fidelity edge.
            grid.entry((0, s.from_x)).or_insert(('╷', s.role));
            grid.entry((height - 1, s.to_x)).or_insert(('╵', s.role));
        }
    }

    let mut rows: Vec<Vec<CanvasCell>> = vec![Vec::new(); height];
    for ((row, column), (glyph, role)) in grid {
        rows[row].push(CanvasCell {
            column,
            glyph,
            role,
        });
    }
    for row in &mut rows {
        row.sort_by_key(|c| c.column);
    }

    Channel {
        height,
        rows,
        dropped,
    }
}

/// Merge two glyphs landing on the same cell: a `│`/`─` collision is a real
/// crossing (`┼`); anything else (two segments genuinely trying to occupy
/// the exact same corner, an already-`┼` cell, ...) keeps the newer glyph
/// rather than panicking -- a rare, cosmetically-imperfect edge case on a
/// dense fixture, not a correctness bug worth failing the whole render
/// over.
fn merge_glyph(existing: char, new: char) -> char {
    if existing == new {
        return existing;
    }
    match (existing, new) {
        ('│', '─') | ('─', '│') => '┼',
        _ => new,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::NodeId;
    use crate::graph::sugiyama::{layout, Layout};

    fn id(name: &str) -> NodeId {
        NodeId::from(name)
    }

    fn label(n: &NodeId) -> String {
        n.to_string()
    }

    fn cell_at(channel: &Channel, row: usize, column: usize) -> Option<&CanvasCell> {
        channel.rows.get(row)?.iter().find(|c| c.column == column)
    }

    fn simple_layout(bands: &[&[&str]], edges: &[(&str, &str)]) -> Layout {
        let bands: Vec<Vec<NodeId>> = bands
            .iter()
            .map(|b| b.iter().map(|n| id(n)).collect())
            .collect();
        let edges: Vec<(NodeId, NodeId)> = edges.iter().map(|(f, t)| (id(f), id(t))).collect();
        layout(&bands, &edges, label)
    }

    #[test]
    fn a_straight_edge_renders_a_vertical_line_the_full_channel_height() {
        // Same-length labels in both (single-node) bands, so the two
        // x-centers coincide exactly and the edge is straight.
        let layout = simple_layout(&[&["aa"], &["bb"]], &[("aa", "bb")]);
        let channels = route_channels(&layout, &id("nobody"));
        assert_eq!(channels.len(), 1);
        let ch = &channels[0];
        let column = layout.bands[0][0].x_center().round() as usize;
        assert!((0..ch.height).all(|row| cell_at(ch, row, column).map(|c| c.glyph) == Some('│')));
    }

    #[test]
    fn a_bent_edge_gets_corner_glyphs_and_a_horizontal_run() {
        let layout = simple_layout(&[&["a", "b"], &["c", "d"]], &[("a", "d")]);
        let channels = route_channels(&layout, &id("nobody"));
        let ch = &channels[0];
        // Some row must contain both a `╮` and a `╯`.
        let has_bend = ch
            .rows
            .iter()
            .any(|row| row.iter().any(|c| c.glyph == '╮') && row.iter().any(|c| c.glyph == '╯'));
        assert!(has_bend, "expected a bend row with both corner glyphs");
    }

    #[test]
    fn diamond_fixture_routes_both_parent_edges_without_panicking() {
        let layout = simple_layout(&[&["a", "b"], &["c"]], &[("a", "c"), ("b", "c")]);
        let channels = route_channels(&layout, &id("nobody"));
        assert_eq!(channels.len(), 1);
        assert!(channels[0].height >= 1);
    }

    #[test]
    fn long_edge_spanning_two_channels_routes_through_the_dummy_hop() {
        let layout = simple_layout(&[&["a"], &["mid"], &["b"]], &[("a", "b")]);
        let channels = route_channels(&layout, &id("nobody"));
        assert_eq!(channels.len(), 2, "two channels for a 3-band layout");
        // Both channels must have at least one cell -- the edge really did
        // route through both hops, not just one.
        assert!(channels[0].rows.iter().any(|r| !r.is_empty()));
        assert!(channels[1].rows.iter().any(|r| !r.is_empty()));
    }

    #[test]
    fn multi_parent_convergence_keeps_every_edge_routed() {
        let layout = simple_layout(
            &[&["p1", "p2", "p3"], &["child"]],
            &[("p1", "child"), ("p2", "child"), ("p3", "child")],
        );
        let channels = route_channels(&layout, &id("nobody"));
        let ch = &channels[0];
        let total_cells: usize = ch.rows.iter().map(Vec::len).sum();
        assert!(total_cells >= 3, "expect cells from all three edges");
    }

    #[test]
    fn focused_node_s_outgoing_edge_is_tagged_focused_outgoing() {
        let layout = simple_layout(&[&["a"], &["b"]], &[("a", "b")]);
        let column = layout.bands[0][0].x_center().round() as usize;
        let channels = route_channels(&layout, &id("a"));
        let cell = cell_at(&channels[0], 0, column).expect("cell present");
        assert_eq!(cell.role, CanvasRole::FocusedOutgoing);
    }

    #[test]
    fn focused_node_s_incoming_edge_is_tagged_focused_incoming() {
        let layout = simple_layout(&[&["a"], &["b"]], &[("a", "b")]);
        let column = layout.bands[0][0].x_center().round() as usize;
        let channels = route_channels(&layout, &id("b"));
        let cell = cell_at(&channels[0], 0, column).expect("cell present");
        assert_eq!(cell.role, CanvasRole::FocusedIncoming);
    }

    #[test]
    fn crossing_segments_render_a_plus_glyph() {
        // A straight vertical at column 5, plus a bent segment whose
        // horizontal run spans columns 0..=10 (crossing column 5 at its own
        // bend row) -- forces a real geometric crossing. Built by hand
        // (rather than through `sugiyama::layout`) so this exercises only
        // `route_channels`'s own crossing-glyph logic: issue #18's median
        // coordinate assignment deliberately straightens most edges that
        // would otherwise cross, so a fixture routed through the real
        // layout algorithm can no longer be relied on to still cross.
        use crate::graph::sugiyama::{RoutedEdge, Slot, SlotId};
        let slot = |name: &str, x: usize, width: usize| Slot {
            id: SlotId::Real(id(name)),
            label: name.to_string(),
            x,
            width,
        };
        let layout = Layout {
            bands: vec![
                vec![slot("straight_top", 5, 1), slot("bent_top", 0, 1)],
                vec![slot("straight_bottom", 5, 1), slot("bent_bottom", 10, 1)],
            ],
            edges: vec![
                RoutedEdge {
                    from: id("straight_top"),
                    to: id("straight_bottom"),
                    waypoints: vec![(0, 5.5), (1, 5.5)],
                },
                RoutedEdge {
                    from: id("bent_top"),
                    to: id("bent_bottom"),
                    waypoints: vec![(0, 0.5), (1, 10.5)],
                },
            ],
        };
        let channels = route_channels(&layout, &id("nobody"));
        let ch = &channels[0];
        let has_crossing = ch.rows.iter().any(|row| row.iter().any(|c| c.glyph == '┼'));
        assert!(
            has_crossing,
            "expected at least one crossing cell in this fixture"
        );
    }

    #[test]
    fn empty_layout_yields_no_channels() {
        let layout = simple_layout(&[&["only"]], &[]);
        let channels = route_channels(&layout, &id("only"));
        assert!(channels.is_empty(), "one band has no channel gap at all");
    }

    /// A hand-built two-band [`Layout`] with `normal_count` overlapping bent
    /// "normal" edges (each sharing the same far endpoint, so every one's
    /// x-range overlaps every other's and greedy scheduling needs a fresh
    /// bend row per edge -- see [`crossing_segments_render_a_plus_glyph`]'s
    /// comment for why hand-building beats routing this through
    /// `sugiyama::layout`), plus one optional focused edge with the same
    /// overlap shape.
    fn overlapping_bent_layout(normal_count: usize, with_focused: bool) -> (Layout, NodeId) {
        use crate::graph::sugiyama::{RoutedEdge, Slot, SlotId};
        let focus = id("focus");
        let far_x = 1000.0;
        let mut top = Vec::new();
        let mut bottom = Vec::new();
        let mut edges = Vec::new();
        if with_focused {
            let name = id("focused_src");
            top.push(Slot {
                id: SlotId::Real(name.clone()),
                label: "focused_src".to_string(),
                x: 0,
                width: 1,
            });
            bottom.push(Slot {
                id: SlotId::Real(focus.clone()),
                label: "focus".to_string(),
                x: 1000,
                width: 1,
            });
            edges.push(RoutedEdge {
                from: name,
                to: focus.clone(),
                waypoints: vec![(0, 0.5), (1, far_x)],
            });
        }
        for i in 0..normal_count {
            let x = (i + 1) * 2;
            let name = id(&format!("n{i}"));
            let target = id(&format!("t{i}"));
            top.push(Slot {
                id: SlotId::Real(name.clone()),
                label: name.to_string(),
                x,
                width: 1,
            });
            bottom.push(Slot {
                id: SlotId::Real(target.clone()),
                label: target.to_string(),
                x: 2000 + x,
                width: 1,
            });
            edges.push(RoutedEdge {
                from: name,
                to: target,
                // Each edge's far endpoint gets a distinct (but still
                // deep-overlapping) x so a dropped edge's bottom stub
                // lands in its own column, not stacked on top of every
                // other dropped edge's stub in the same cell.
                waypoints: vec![(0, x as f32 + 0.5), (1, far_x + i as f32)],
            });
        }
        (
            Layout {
                bands: vec![top, bottom],
                edges,
            },
            focus,
        )
    }

    #[test]
    fn under_budget_channel_is_unchanged() {
        let (layout, focus) = overlapping_bent_layout(CHANNEL_HEIGHT_BUDGET - 1, false);
        let channels = route_channels(&layout, &focus);
        let ch = &channels[0];
        assert_eq!(ch.dropped, 0);
        assert_eq!(ch.height, CHANNEL_HEIGHT_BUDGET - 1);
    }

    #[test]
    fn over_budget_channel_degrades_normal_edges_and_reports_the_dropped_count() {
        let extra = 3;
        let (layout, focus) = overlapping_bent_layout(CHANNEL_HEIGHT_BUDGET + extra, false);
        let channels = route_channels(&layout, &focus);
        let ch = &channels[0];
        assert_eq!(ch.dropped, extra, "exactly the over-budget edges drop");
        assert_eq!(ch.height, CHANNEL_HEIGHT_BUDGET);
        // Whichever edge legitimately occupies row 0 necessarily has a
        // `hi` reaching at least as far as every dropped edge's `lo` --
        // that's the very reason the drop happened (no row was free) --
        // so a dropped edge's *top* stub is mathematically guaranteed to
        // land inside that occupant's own horizontal run and never render
        // as a separate visible glyph in this fixture shape (a stub never
        // overwrites real content -- see `route_one_channel`'s doc). The
        // *bottom* stub's column is each edge's own far endpoint, which
        // this fixture deliberately spreads out past the in-budget rows'
        // own far reach, so it isn't subject to the same guarantee -- it
        // is what actually proves the degrade path renders something.
        let stub_bottoms = ch.rows[ch.height - 1]
            .iter()
            .filter(|c| c.glyph == '╵')
            .count();
        assert!(
            stub_bottoms >= 1,
            "expected at least one visible bottom stub"
        );
    }

    #[test]
    fn over_budget_channel_keeps_the_focused_edge_at_full_fidelity() {
        let (layout, focus) = overlapping_bent_layout(CHANNEL_HEIGHT_BUDGET + 3, true);
        let channels = route_channels(&layout, &focus);
        let ch = &channels[0];
        // The focused edge (`focused_src` -> `focus`, so `FocusedIncoming`
        // from `focus`'s point of view) must still get a real bend row (a
        // `╮`/`╯` pair), never degraded to a stub, regardless of how many
        // normal edges also want channel height.
        let focused_bend = ch.rows.iter().any(|row| {
            row.iter()
                .any(|c| c.glyph == '╮' && c.role == CanvasRole::FocusedIncoming)
        });
        assert!(
            focused_bend,
            "focused edge must keep its bend row even over budget"
        );
        let focused_stub = ch.rows.iter().any(|row| {
            row.iter().any(|c| {
                c.role == CanvasRole::FocusedIncoming && (c.glyph == '╷' || c.glyph == '╵')
            })
        });
        assert!(!focused_stub, "focused edge must never degrade to a stub");
    }

    #[test]
    fn folded_aggregated_edge_between_namespace_rows_routes_cleanly() {
        let layout = simple_layout(&[&["ns_a"], &["ns_b"]], &[("ns_a", "ns_b")]);
        let channels = route_channels(&layout, &id("ns_a"));
        assert_eq!(channels.len(), 1);
        assert!(channels[0].rows.iter().any(|r| !r.is_empty()));
    }
}
