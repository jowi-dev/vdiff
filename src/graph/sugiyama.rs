//! Pure, toolkit-neutral layered (Sugiyama-style) DAG layout for the
//! `--tui` canvas graph screen (issue #17): turns an ordered list of bands
//! (top-to-bottom layers, e.g. [`crate::core::app::App::layers`] filtered
//! through the fold-by-namespace zoom -- see
//! [`crate::core::rail_view::visible_rows_with_layers`]) plus the edges
//! between them into char-unit x-coordinates and a routable edge list.
//! No `ratatui`/terminal types here -- [`crate::graph::canvas`] turns this
//! into an actual char+role cell grid, the same split
//! [`crate::graph::rails`] keeps from `crate::tui::render`.
//!
//! # The algorithm
//!
//! 1. **Dummy-node insertion**: an edge spanning more than one band gets a
//!    synthetic [`SlotId::Dummy`] inserted into every band strictly between
//!    its two endpoints' bands, chaining the edge through consecutive
//!    adjacent-band hops only -- exactly the classic Sugiyama trick that
//!    turns "route this long edge" into "route several short ones", which
//!    is also what makes per-channel routing in [`crate::graph::canvas`]
//!    tractable (see that module's doc). Like [`crate::graph::rails`], a
//!    span is resolved by *band index order*, not by which endpoint is the
//!    dependency's `from`/`to` -- see [`Self::edges`]'s own doc for why an
//!    edge can point "upward" in band order once fold-collapse is in play.
//! 2. **Ordering**: a few barycenter sweeps (alternating downward and
//!    upward through the band list) reorder each band's slots by the mean
//!    position of their already-placed neighbors in the adjacent band,
//!    the standard crossing-reduction heuristic behind every layered-graph
//!    renderer (see the ascii-dag/Graphviz `dot` prior art cited in the
//!    issue). A slot with no neighbors in the direction being swept keeps
//!    its current position (stable sort), so isolated nodes don't drift.
//! 3. **Coordinate assignment**: once ordering is fixed, a few more
//!    downward/upward sweeps ([`assign_coordinates`]) align each slot to
//!    the mean x-center of its already-placed neighbors in the adjacent
//!    band, resolving any resulting overlap by pushing slots apart while
//!    strictly preserving the band's left-to-right order
//!    ([`resolve_band_positions`]). A dummy's own aligned position wins
//!    over a real slot's when the two disagree -- see that function's
//!    doc -- because a straightened dummy chain is what makes a long edge
//!    read as one vertical rail instead of a long horizontal jog; this is
//!    the fix that actually collapses inter-band channel height (see
//!    [`crate::graph::canvas`]'s own doc for the routing side of that).
//!    Bands end up sparse with gaps between slots -- that's the intended
//!    effect, not a bug. Before this pass existed, `x` was just a running
//!    left-packed offset per band with no idea what was above or below,
//!    which is what made nearly every edge in a real change set span a
//!    long horizontal distance (see the issue's own real-use screenshot).
//!
//! Band-wrap (splitting an overflowing band into multiple node rows) is
//! deliberately *not* handled here -- it needs to know the actual terminal
//! width, which this module doesn't take at all (see [`layout`]'s doc), so
//! it's [`crate::graph::canvas`]'s job, over this module's unbounded
//! char-space output.

use std::collections::HashMap;

use crate::graph::model::NodeId;

/// One placed slot's identity: a real node, or a synthetic routing point
/// for an edge spanning more than one band -- `edge` is that edge's index
/// into the `edges` slice [`layout`] was called with, `band` is which
/// intermediate band this particular hop sits in (both together make a
/// dummy's identity unique even when two long edges pass through the same
/// band).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SlotId {
    Real(NodeId),
    Dummy(usize, usize),
}

impl SlotId {
    /// The real node id this slot names, or `None` for a [`SlotId::Dummy`].
    pub fn real_id(&self) -> Option<&NodeId> {
        match self {
            SlotId::Real(id) => Some(id),
            SlotId::Dummy(..) => None,
        }
    }

    pub fn is_dummy(&self) -> bool {
        matches!(self, SlotId::Dummy(..))
    }
}

/// One placed node (real or dummy) in a band, in char units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub id: SlotId,
    /// Display label -- empty for a dummy.
    pub label: String,
    /// Starting column within the band.
    pub x: usize,
    /// Label width in characters (`1` for a dummy).
    pub width: usize,
}

impl Slot {
    /// The column this slot's edges should route through -- the label's
    /// horizontal midpoint, in half-character units collapsed to `f32` so
    /// [`crate::core::focus::move_focus`] (which already works in `f32`
    /// x-centers over the GUI's pixel layout) can be reused unchanged over
    /// this char-space layout too.
    pub fn x_center(&self) -> f32 {
        self.x as f32 + (self.width as f32) / 2.0
    }
}

/// One dependency edge, routed through however many bands it spans as a
/// sequence of `(band_index, x_center)` waypoints in ascending band order
/// (not necessarily `from`'s band to `to`'s band -- see the module doc).
/// The first and last waypoints are the two real endpoints' own centers;
/// anything in between is a dummy hop.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub waypoints: Vec<(usize, f32)>,
}

/// [`layout`]'s output: one band of [`Slot`]s (left-to-right order, char
/// units, unbounded width -- see the module doc on why wrapping isn't done
/// here) and the fully routed edge list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    pub bands: Vec<Vec<Slot>>,
    pub edges: Vec<RoutedEdge>,
}

/// Horizontal gap in characters between two adjacent slots in a band.
const GAP: usize = 2;

/// How many barycenter ordering sweeps [`layout`] runs (each sweep is one
/// downward pass plus one upward pass) -- enough for the fixture sizes this
/// crate targets (15-40 nodes, see the issue's sizing note) to settle;
/// unlike a full min-crossing solver this never claims optimality, just a
/// few rounds of local improvement.
const SWEEPS: usize = 4;

/// Lay out `bands` (top-to-bottom, one node-id list per layer) and `edges`
/// (pairs of ids, each expected to appear somewhere in `bands` -- an edge
/// with an endpoint missing from every band, or with both endpoints in the
/// same band, is silently dropped, mirroring
/// [`crate::graph::rails::compute`]'s own defensiveness). `label_of` maps a
/// real node id to its display label (the caller decides exactly what that
/// looks like -- status bullet, badges, etc. -- this module only needs its
/// length).
pub fn layout(
    bands_in: &[Vec<NodeId>],
    edges: &[(NodeId, NodeId)],
    label_of: impl Fn(&NodeId) -> String,
) -> Layout {
    let band_of: HashMap<&NodeId, usize> = bands_in
        .iter()
        .enumerate()
        .flat_map(|(i, band)| band.iter().map(move |id| (id, i)))
        .collect();

    let mut order: Vec<Vec<SlotId>> = bands_in
        .iter()
        .map(|band| band.iter().cloned().map(SlotId::Real).collect())
        .collect();

    // Build each edge's dummy chain, inserting a `SlotId::Dummy` into every
    // band strictly between its two endpoints' bands (in band-index order,
    // not `from`/`to` order -- see the module doc).
    struct Chain {
        from: NodeId,
        to: NodeId,
        top_band: usize,
        slots: Vec<SlotId>,
    }
    let mut chains: Vec<Chain> = Vec::new();
    for (edge_idx, (from, to)) in edges.iter().enumerate() {
        let (Some(&band_from), Some(&band_to)) = (band_of.get(from), band_of.get(to)) else {
            continue;
        };
        if band_from == band_to {
            continue;
        }
        let (top_band, bottom_band) = (band_from.min(band_to), band_from.max(band_to));
        let top_id = if band_from <= band_to { from } else { to };
        let bottom_id = if band_from <= band_to { to } else { from };

        let mut slots = vec![SlotId::Real(top_id.clone())];
        for (band, band_order) in order
            .iter_mut()
            .enumerate()
            .take(bottom_band)
            .skip(top_band + 1)
        {
            let dummy = SlotId::Dummy(edge_idx, band);
            band_order.push(dummy.clone());
            slots.push(dummy);
        }
        slots.push(SlotId::Real(bottom_id.clone()));

        chains.push(Chain {
            from: from.clone(),
            to: to.clone(),
            top_band,
            slots,
        });
    }

    // Adjacency between consecutive bands: `pairs[i]` is every
    // `(upper_slot, lower_slot)` connecting band `i` to band `i+1`.
    let band_count = order.len();
    let mut pairs: Vec<Vec<(SlotId, SlotId)>> = vec![Vec::new(); band_count.saturating_sub(1)];
    for chain in &chains {
        for k in 0..chain.slots.len().saturating_sub(1) {
            let band = chain.top_band + k;
            pairs[band].push((chain.slots[k].clone(), chain.slots[k + 1].clone()));
        }
    }

    barycenter_sweeps(&mut order, &pairs);

    // Resolve each slot's label/width up front (needed by coordinate
    // assignment below, and again for the final `Slot`s), preserving the
    // now-fixed band order.
    let labels: Vec<Vec<(SlotId, String, usize)>> = order
        .iter()
        .map(|ids| {
            ids.iter()
                .map(|id| {
                    let label = match id.real_id() {
                        Some(real) => label_of(real),
                        None => String::new(),
                    };
                    let width = label.chars().count().max(1);
                    (id.clone(), label, width)
                })
                .collect()
        })
        .collect();

    let centers = assign_coordinates(&order, &labels, &pairs);

    // Build the final `Slot`s from each band's resolved centers: left edge
    // is the center minus half the width, then a left-to-right integer
    // clean-up pass guarantees no rounding-induced overlap ever survives
    // (see `assign_coordinates`'s doc on why this is still needed even
    // though the float pass itself never overlaps).
    let mut x_center_of: HashMap<(usize, SlotId), f32> = HashMap::new();
    let mut bands: Vec<Vec<Slot>> = Vec::with_capacity(band_count);
    for (band_idx, sized) in labels.into_iter().enumerate() {
        let band_centers = &centers[band_idx];
        let mut slots = Vec::with_capacity(sized.len());
        let mut min_x = 0i64;
        for (i, (id, label, width)) in sized.into_iter().enumerate() {
            let desired_x = (band_centers[i] - (width as f32) / 2.0).round() as i64;
            let x = desired_x.max(min_x).max(0) as usize;
            min_x = x as i64 + width as i64 + GAP as i64;
            let slot = Slot {
                id: id.clone(),
                label,
                x,
                width,
            };
            x_center_of.insert((band_idx, id), slot.x_center());
            slots.push(slot);
        }
        bands.push(slots);
    }

    let routed_edges = chains
        .into_iter()
        .map(|chain| {
            let waypoints = chain
                .slots
                .iter()
                .enumerate()
                .map(|(k, slot_id)| {
                    let band = chain.top_band + k;
                    let center = x_center_of
                        .get(&(band, slot_id.clone()))
                        .copied()
                        .unwrap_or(0.0);
                    (band, center)
                })
                .collect();
            RoutedEdge {
                from: chain.from,
                to: chain.to,
                waypoints,
            }
        })
        .collect();

    Layout {
        bands,
        edges: routed_edges,
    }
}

/// Run [`SWEEPS`] rounds of downward-then-upward barycenter reordering over
/// `order`, consulting `pairs` (band-to-band adjacency) for each slot's
/// neighbors in the band being swept from.
fn barycenter_sweeps(order: &mut [Vec<SlotId>], pairs: &[Vec<(SlotId, SlotId)>]) {
    if order.len() < 2 {
        return;
    }
    for _ in 0..SWEEPS {
        for band in 1..order.len() {
            reorder_band(order, pairs, band, band - 1, true);
        }
        for band in (0..order.len() - 1).rev() {
            reorder_band(order, pairs, band, band + 1, false);
        }
    }
}

/// Reorder `order[band]` by the mean position, in `order[neighbor_band]`,
/// of each slot's neighbors on that side (upper neighbors when
/// `neighbor_is_upper`, lower otherwise) -- slots with no such neighbor
/// keep their current relative position (stable sort on the original
/// index as a tiebreak, so isolated nodes never drift).
fn reorder_band(
    order: &mut [Vec<SlotId>],
    pairs: &[Vec<(SlotId, SlotId)>],
    band: usize,
    neighbor_band: usize,
    neighbor_is_upper: bool,
) {
    let neighbor_pos: HashMap<&SlotId, usize> = order[neighbor_band]
        .iter()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();

    let pair_band = if neighbor_is_upper { band - 1 } else { band };
    let mut neighbors_of: HashMap<&SlotId, Vec<usize>> = HashMap::new();
    if let Some(edge_list) = pairs.get(pair_band) {
        for (upper, lower) in edge_list {
            let (this_slot, other_slot) = if neighbor_is_upper {
                (lower, upper)
            } else {
                (upper, lower)
            };
            if let Some(&pos) = neighbor_pos.get(other_slot) {
                neighbors_of.entry(this_slot).or_default().push(pos);
            }
        }
    }

    let current: Vec<SlotId> = order[band].clone();
    let mut keyed: Vec<(f64, usize, SlotId)> = current
        .into_iter()
        .enumerate()
        .map(|(idx, id)| {
            let barycenter = neighbors_of
                .get(&id)
                .map(|positions| positions.iter().sum::<usize>() as f64 / positions.len() as f64);
            (barycenter.unwrap_or(idx as f64), idx, id)
        })
        .collect();
    keyed.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    order[band] = keyed.into_iter().map(|(_, _, id)| id).collect();
}

/// Run [`SWEEPS`] rounds of downward-then-upward coordinate-assignment
/// sweeps over `order` (already ordering-settled by [`barycenter_sweeps`]),
/// returning each band's final x-centers in the same shape as `order`
/// itself -- this is issue #18's load-bearing fix: without it, x is a
/// naive left-packed running offset with no idea what's above or below,
/// so nearly every edge spans a long horizontal distance regardless of how
/// well [`barycenter_sweeps`] ordered things. Each sweep aligns every slot
/// to the mean x-center of its already-placed neighbors in the adjacent
/// band (see [`band_desired_centers`]), then [`resolve_band_positions`]
/// turns those "wanted" positions into an actual non-overlapping,
/// order-preserving placement for the band -- a dummy's wanted position
/// wins outright (real slots average toward theirs instead), which is
/// what lets a straightened dummy chain hold a single x column: the real
/// endpoints pull on the dummies, but the dummies don't get diluted by
/// also being pulled on by unrelated siblings the way a real node with
/// several neighbors would.
fn assign_coordinates(
    order: &[Vec<SlotId>],
    labels: &[Vec<(SlotId, String, usize)>],
    pairs: &[Vec<(SlotId, SlotId)>],
) -> Vec<Vec<f32>> {
    let band_count = order.len();
    let widths: Vec<Vec<usize>> = labels
        .iter()
        .map(|band| band.iter().map(|(_, _, w)| *w).collect())
        .collect();
    let is_dummy: Vec<Vec<bool>> = order
        .iter()
        .map(|band| band.iter().map(SlotId::is_dummy).collect())
        .collect();

    // Initial centers: the same naive left-pack the pre-fix code always
    // produced, used both as the coordinate sweeps' starting point and as
    // the fallback "desired" position for a slot with no neighbors in the
    // direction currently being swept (so an isolated slot never collapses
    // toward zero -- mirrors `reorder_band`'s own stable fallback for
    // ordering).
    let mut centers: Vec<Vec<f32>> = widths
        .iter()
        .map(|band| {
            let mut running_x = 0usize;
            band.iter()
                .map(|&w| {
                    let center = running_x as f32 + (w as f32) / 2.0;
                    running_x += w + GAP;
                    center
                })
                .collect()
        })
        .collect();

    if band_count < 2 {
        return centers;
    }

    for _ in 0..SWEEPS {
        for band in 1..band_count {
            let desired = band_desired_centers(
                &order[band],
                &centers[band],
                &order[band - 1],
                &centers[band - 1],
                &pairs[band - 1],
                true,
            );
            centers[band] = resolve_band_positions(&desired, &is_dummy[band], &widths[band]);
        }
        for band in (0..band_count - 1).rev() {
            let desired = band_desired_centers(
                &order[band],
                &centers[band],
                &order[band + 1],
                &centers[band + 1],
                &pairs[band],
                false,
            );
            centers[band] = resolve_band_positions(&desired, &is_dummy[band], &widths[band]);
        }
    }

    centers
}

/// Each slot's "desired" x-center in `this_order`/`this_centers`: the mean
/// x-center of its neighbors in `neighbor_order`/`neighbor_centers`
/// (looked up via `pair_list`, the same adjacency [`reorder_band`] already
/// consults for ordering), falling back to the slot's own current center
/// when it has no neighbor on that side at all. `neighbor_is_upper`
/// matches [`reorder_band`]'s own parameter: `true` when `neighbor_order`
/// is the band above (a downward sweep), `false` when it's the band below.
fn band_desired_centers(
    this_order: &[SlotId],
    this_centers: &[f32],
    neighbor_order: &[SlotId],
    neighbor_centers: &[f32],
    pair_list: &[(SlotId, SlotId)],
    neighbor_is_upper: bool,
) -> Vec<f32> {
    let neighbor_center_of: HashMap<&SlotId, f32> = neighbor_order
        .iter()
        .zip(neighbor_centers.iter())
        .map(|(id, &c)| (id, c))
        .collect();

    let mut neighbors_of: HashMap<&SlotId, Vec<f32>> = HashMap::new();
    for (upper, lower) in pair_list {
        let (this_slot, other_slot) = if neighbor_is_upper {
            (lower, upper)
        } else {
            (upper, lower)
        };
        if let Some(&center) = neighbor_center_of.get(other_slot) {
            neighbors_of.entry(this_slot).or_default().push(center);
        }
    }

    this_order
        .iter()
        .enumerate()
        .map(|(i, id)| match neighbors_of.get(id) {
            Some(centers) => centers.iter().sum::<f32>() / centers.len() as f32,
            None => this_centers[i],
        })
        .collect()
}

/// Turn one band's "desired" x-centers into an actual placement that never
/// overlaps and never reorders `desired`'s own left-to-right index order --
/// the two invariants [`assign_coordinates`] depends on every sweep,
/// regardless of how contradictory the desired positions are.
///
/// Runs a left-to-right pass (push right just enough to keep the minimum
/// gap from the previous slot) and a right-to-left pass (the mirror,
/// pushing left), then blends them: a dummy slot trusts its own `desired`
/// value directly (see [`assign_coordinates`]'s doc on why -- this is the
/// "real nodes yield to dummies" priority the issue asks for), a real slot
/// takes the average of the two directional passes. That blend can still
/// violate the minimum gap once rounding/averaging interact, so a final
/// left-to-right clean-up pass re-enforces it -- this is also what
/// ultimately guarantees [`Slot`]s built from this never overlap, letting
/// the integer stage in [`layout`] be a pure rounding step rather than a
/// second place overlaps could sneak back in.
fn resolve_band_positions(desired: &[f32], is_dummy: &[bool], widths: &[usize]) -> Vec<f32> {
    let n = desired.len();
    if n == 0 {
        return Vec::new();
    }
    let min_gap =
        |i: usize| -> f32 { (widths[i] as f32) / 2.0 + GAP as f32 + (widths[i + 1] as f32) / 2.0 };

    let mut left = vec![0.0f32; n];
    let mut cursor = f32::NEG_INFINITY;
    for (i, left_slot) in left.iter_mut().enumerate() {
        let min_allowed = if i == 0 {
            f32::NEG_INFINITY
        } else {
            cursor + min_gap(i - 1)
        };
        *left_slot = desired[i].max(min_allowed);
        cursor = *left_slot;
    }

    let mut right = vec![0.0f32; n];
    let mut cursor = f32::INFINITY;
    for i in (0..n).rev() {
        let max_allowed = if i == n - 1 {
            f32::INFINITY
        } else {
            cursor - min_gap(i)
        };
        right[i] = desired[i].min(max_allowed);
        cursor = right[i];
    }

    let mut blended: Vec<f32> = (0..n)
        .map(|i| {
            if is_dummy[i] {
                desired[i]
            } else {
                (left[i] + right[i]) / 2.0
            }
        })
        .collect();

    let mut cursor = f32::NEG_INFINITY;
    for (i, slot) in blended.iter_mut().enumerate() {
        let min_allowed = if i == 0 {
            f32::NEG_INFINITY
        } else {
            cursor + min_gap(i - 1)
        };
        *slot = slot.max(min_allowed);
        cursor = *slot;
    }
    blended
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> NodeId {
        NodeId::from(name)
    }

    fn band(names: &[&str]) -> Vec<NodeId> {
        names.iter().map(|n| id(n)).collect()
    }

    fn edge(from: &str, to: &str) -> (NodeId, NodeId) {
        (id(from), id(to))
    }

    fn short_label(n: &NodeId) -> String {
        n.to_string()
    }

    fn slot_x_center(bands: &[Vec<Slot>], band: usize, name: &str) -> f32 {
        bands[band]
            .iter()
            .find(|s| s.id.real_id() == Some(&id(name)))
            .map(Slot::x_center)
            .unwrap_or_else(|| panic!("{name} not found in band {band}"))
    }

    #[test]
    fn straight_edge_needs_no_dummy_and_has_two_waypoints() {
        let bands = vec![band(&["a"]), band(&["b"])];
        let edges = vec![edge("a", "b")];
        let layout = layout(&bands, &edges, short_label);
        assert_eq!(layout.edges.len(), 1);
        assert_eq!(layout.edges[0].waypoints.len(), 2);
        assert_eq!(
            layout.bands[1].len(),
            1,
            "no dummy inserted for a 1-band edge"
        );
    }

    #[test]
    fn long_edge_spanning_two_bands_gets_one_dummy_in_the_middle_band() {
        let bands = vec![band(&["a"]), band(&["mid"]), band(&["b"])];
        let edges = vec![edge("a", "b")];
        let layout = layout(&bands, &edges, short_label);

        assert_eq!(layout.bands[1].len(), 2, "mid band gains one dummy slot");
        assert!(layout.bands[1].iter().any(|s| s.id.is_dummy()));
        assert_eq!(layout.edges[0].waypoints.len(), 3);
    }

    #[test]
    fn dummy_slots_have_empty_labels_and_width_one() {
        let bands = vec![band(&["a"]), band(&["mid"]), band(&["b"])];
        let edges = vec![edge("a", "b")];
        let layout = layout(&bands, &edges, short_label);
        let dummy = layout.bands[1]
            .iter()
            .find(|s| s.id.is_dummy())
            .expect("dummy present");
        assert_eq!(dummy.label, "");
        assert_eq!(dummy.width, 1);
    }

    #[test]
    fn diamond_shape_places_both_parents_before_the_shared_child() {
        // a -> c, b -> c : a classic diamond top (a, b share a band) into
        // one bottom node.
        let bands = vec![band(&["a", "b"]), band(&["c"])];
        let edges = vec![edge("a", "c"), edge("b", "c")];
        let layout = layout(&bands, &edges, short_label);
        assert_eq!(layout.bands[0].len(), 2);
        assert_eq!(layout.bands[1].len(), 1);
        assert_eq!(layout.edges.len(), 2);
    }

    #[test]
    fn multi_parent_convergence_keeps_every_parent_edge() {
        let bands = vec![band(&["p1", "p2", "p3"]), band(&["child"])];
        let edges = vec![
            edge("p1", "child"),
            edge("p2", "child"),
            edge("p3", "child"),
        ];
        let layout = layout(&bands, &edges, short_label);
        assert_eq!(layout.edges.len(), 3);
    }

    #[test]
    fn an_edge_with_an_unknown_endpoint_is_dropped_without_panicking() {
        let bands = vec![band(&["a"]), band(&["b"])];
        let edges = vec![edge("a", "ghost")];
        let layout = layout(&bands, &edges, short_label);
        assert!(layout.edges.is_empty());
    }

    #[test]
    fn a_same_band_edge_is_dropped() {
        let bands = vec![band(&["a", "b"])];
        let edges = vec![edge("a", "b")];
        let layout = layout(&bands, &edges, short_label);
        assert!(layout.edges.is_empty());
    }

    #[test]
    fn an_upward_edge_between_bands_still_routes_through_the_middle_band() {
        // Mirrors `crate::graph::rails`' own upward-edge fixture: after
        // fold-collapse, a visible edge can point from a deeper band index
        // to a shallower one.
        let bands = vec![band(&["a"]), band(&["mid"]), band(&["b"])];
        let edges = vec![edge("b", "a")];
        let layout = layout(&bands, &edges, short_label);
        assert_eq!(
            layout.bands[1].len(),
            2,
            "still gets a dummy in the middle band"
        );
        assert_eq!(layout.edges[0].waypoints.len(), 3);
    }

    #[test]
    fn ordering_sweeps_settle_without_panicking_on_a_wider_fixture() {
        // Enough bands/nodes/edges to exercise multiple sweep rounds
        // without asserting an exact ordering (barycenter ordering has no
        // single "correct" answer worth pinning to a fragile assertion).
        let bands = vec![
            band(&["a1", "a2", "a3"]),
            band(&["b1", "b2"]),
            band(&["c1", "c2", "c3"]),
        ];
        let edges = vec![
            edge("a1", "b1"),
            edge("a2", "b1"),
            edge("a2", "b2"),
            edge("a3", "b2"),
            edge("b1", "c1"),
            edge("b1", "c2"),
            edge("b2", "c3"),
        ];
        let layout = layout(&bands, &edges, short_label);
        assert_eq!(layout.bands.len(), 3);
        assert_eq!(layout.edges.len(), 7);
    }

    #[test]
    fn isolated_node_with_no_edges_keeps_its_position_across_sweeps() {
        let bands = vec![band(&["a", "isolated", "b"]), band(&["target"])];
        let edges = vec![edge("a", "target"), edge("b", "target")];
        let layout = layout(&bands, &edges, short_label);
        // `isolated` has no neighbors at all, so its barycenter sort key
        // falls back to its original index -- it should stay somewhere in
        // the band, not vanish or panic.
        assert!(layout.bands[0]
            .iter()
            .any(|s| s.id.real_id() == Some(&id("isolated"))));
    }

    #[test]
    fn x_positions_are_monotonically_increasing_left_to_right_in_a_band() {
        let bands = vec![band(&["a", "b", "c"])];
        let layout = layout(&bands, &[], short_label);
        let xs: Vec<usize> = layout.bands[0].iter().map(|s| s.x).collect();
        for i in 1..xs.len() {
            assert!(xs[i] > xs[i - 1], "slots must not overlap: {xs:?}");
        }
    }

    #[test]
    fn child_under_two_parents_lands_horizontally_between_them() {
        // a, b share a band; c is their only child in the band below. With
        // median/mean coordinate assignment, c's x-center should settle
        // between a's and b's, not left-packed hard against whichever one
        // ordering happened to put first.
        let bands = vec![band(&["a", "b"]), band(&["c"])];
        let edges = vec![edge("a", "c"), edge("b", "c")];
        let layout = layout(&bands, &edges, short_label);
        let a_center = slot_x_center(&layout.bands, 0, "a");
        let b_center = slot_x_center(&layout.bands, 0, "b");
        let c_center = slot_x_center(&layout.bands, 1, "c");
        let (lo, hi) = (a_center.min(b_center), a_center.max(b_center));
        assert!(
            c_center >= lo && c_center <= hi,
            "expected c ({c_center}) between a/b ({lo}..{hi})"
        );
    }

    #[test]
    fn a_straightened_dummy_chain_occupies_a_single_x_column() {
        // A long edge spanning four bands with nothing else contending for
        // space in the middle two bands should settle every dummy hop (and
        // both real endpoints, since each is alone in its own band) at the
        // exact same x-center -- a straight vertical drop.
        let bands = vec![band(&["a"]), band(&[]), band(&[]), band(&["b"])];
        let edges = vec![edge("a", "b")];
        let layout = layout(&bands, &edges, short_label);
        let a_center = slot_x_center(&layout.bands, 0, "a");
        let b_center = slot_x_center(&layout.bands, 3, "b");
        let dummy1_center = layout.bands[1]
            .iter()
            .find(|s| s.id.is_dummy())
            .expect("dummy in mid1")
            .x_center();
        let dummy2_center = layout.bands[2]
            .iter()
            .find(|s| s.id.is_dummy())
            .expect("dummy in mid2")
            .x_center();
        assert_eq!(a_center, b_center);
        assert_eq!(dummy1_center, a_center);
        assert_eq!(dummy2_center, a_center);
    }

    #[test]
    fn coordinate_assignment_preserves_band_order_and_never_overlaps() {
        // A denser fixture (multiple bands, crossing edges) exercising
        // several coordinate-assignment sweeps: after layout, every band's
        // slots must still be in strictly increasing x with at least a
        // full-width gap between consecutive slots -- coordinate assignment
        // must never reorder or overlap what the barycenter ordering pass
        // already settled.
        let bands = vec![
            band(&["a1", "a2", "a3"]),
            band(&["b1", "b2"]),
            band(&["c1", "c2", "c3"]),
        ];
        let edges = vec![
            edge("a1", "b1"),
            edge("a2", "b1"),
            edge("a2", "b2"),
            edge("a3", "b2"),
            edge("b1", "c1"),
            edge("b1", "c2"),
            edge("b2", "c3"),
        ];
        let layout = layout(&bands, &edges, short_label);
        for band in &layout.bands {
            for pair in band.windows(2) {
                let left_end = pair[0].x + pair[0].width;
                assert!(
                    pair[1].x >= left_end + GAP,
                    "slots must keep at least GAP characters apart: {:?}",
                    band
                );
            }
        }
    }

    #[test]
    fn folded_aggregated_edge_between_two_collapsed_namespace_rows_lays_out_cleanly() {
        // Simulates what `crate::core::rail_view::collapse_edges` hands the
        // canvas: two namespace ids standing in for whole subtrees, with a
        // single aggregated edge between them.
        let bands = vec![band(&["ns_a"]), band(&["ns_b"])];
        let edges = vec![edge("ns_a", "ns_b")];
        let layout = layout(&bands, &edges, short_label);
        assert_eq!(layout.edges.len(), 1);
        let a_center = slot_x_center(&layout.bands, 0, "ns_a");
        let b_center = slot_x_center(&layout.bands, 1, "ns_b");
        assert_eq!(layout.edges[0].waypoints[0], (0, a_center));
        assert_eq!(layout.edges[0].waypoints[1], (1, b_center));
    }
}
