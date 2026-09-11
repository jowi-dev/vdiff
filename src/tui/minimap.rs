//! The plane/canvas views' corner minimap model (issue #31): a downscaled
//! map of the full layout extent with the current scroll viewport outlined,
//! so a graph larger than the terminal stays orientable while panning.
//!
//! This module is pure geometry -- layout-space rects in, map-space cells
//! out -- so the whole scale/clamp policy is unit-testable without a
//! terminal. Painting (glyphs, styles, the corner placement itself) lives in
//! [`crate::tui::render`], the same split [`crate::graph::plane`] /
//! `draw_plane_graph` already use.
//!
//! Scale policy: each axis is divided independently by the smallest integer
//! factor that fits the content into [`MAX_INTERIOR_WIDTH`] x
//! [`MAX_INTERIOR_HEIGHT`] cells (ceiling division, never below one), and
//! the map interior then shrinks to exactly what the scaled content needs --
//! a layout only slightly larger than the viewport gets a small map, not a
//! fixed-size one padded with dead cells. Aspect ratio is deliberately not
//! preserved: terminal cells are ~2:1 tall, so a faithful aspect map would
//! either waste the tiny cell budget or drop rows; per-axis fit keeps every
//! occupied row representable.

use crate::graph::plane::Rect;
use std::collections::HashSet;

/// Upper bound on the map interior's width in cells, border excluded.
/// Together with [`MAX_INTERIOR_HEIGHT`] this caps the pane's screen cost
/// (the issue's stated concern) at a corner patch of the graph area.
pub const MAX_INTERIOR_WIDTH: usize = 24;

/// Upper bound on the map interior's height in cells, border excluded.
pub const MAX_INTERIOR_HEIGHT: usize = 10;

/// The minimap's render-ready model: an `width` x `height` cell grid (map
/// space, `(0, 0)` top-left), the cells any layout row covers, the focused
/// row's own cell, and the scroll viewport's rect mapped into the same cell
/// space (already clamped to the grid).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minimap {
    pub width: usize,
    pub height: usize,
    pub occupied: HashSet<(usize, usize)>,
    pub focused: Option<(usize, usize)>,
    pub viewport: Rect,
}

impl Minimap {
    /// Whether map cell `(x, y)` lies on the viewport rect's outline ring --
    /// the border the painter accents so the visible window reads as a
    /// frame, not a filled patch (which would drown the occupancy dots
    /// under it). A viewport mapped down to a single cell row/column
    /// degrades to every cell being outline, which is exactly right at that
    /// scale.
    pub fn on_viewport_outline(&self, x: usize, y: usize) -> bool {
        let vp = &self.viewport;
        let inside = x >= vp.x && x < vp.x + vp.w && y >= vp.y && y < vp.y + vp.h;
        let on_edge = x == vp.x || x + 1 == vp.x + vp.w || y == vp.y || y + 1 == vp.y + vp.h;
        inside && on_edge
    }
}

/// Map layout-space span `[start, start + len)` down by `scale` into the
/// inclusive map-cell range `(first, last)`, clamped to `0..max_cells`.
/// A zero-length span still covers its own start cell -- degenerate rects
/// shouldn't vanish from the map entirely.
fn scale_span(start: usize, len: usize, scale: usize, max_cells: usize) -> (usize, usize) {
    let last_cell = max_cells.saturating_sub(1);
    let first = (start / scale).min(last_cell);
    let last = ((start + len.max(1) - 1) / scale).min(last_cell);
    (first, last)
}

/// Build the minimap model, or `None` when the map isn't worth drawing: the
/// content already fits the viewport in both dimensions (nothing to orient
/// by), or the content is empty. `content_w`/`content_h` are the layout's
/// full extent in layout-space cells, `rows` the occupied row rects,
/// `focused` the focused row's rect (if it has one this frame), `viewport`
/// the currently visible window (`scroll_x`, `scroll_y`, area width/height)
/// in the same layout space. `max_w`/`max_h` are the interior cell budget --
/// callers pass [`MAX_INTERIOR_WIDTH`]/[`MAX_INTERIOR_HEIGHT`]; tests pass
/// smaller budgets.
pub fn build(
    content_w: usize,
    content_h: usize,
    rows: &[Rect],
    focused: Option<Rect>,
    viewport: Rect,
    max_w: usize,
    max_h: usize,
) -> Option<Minimap> {
    if content_w == 0 || content_h == 0 || max_w == 0 || max_h == 0 {
        return None;
    }
    if viewport.w >= content_w && viewport.h >= content_h {
        return None;
    }

    let scale_x = content_w.div_ceil(max_w).max(1);
    let scale_y = content_h.div_ceil(max_h).max(1);
    let width = content_w.div_ceil(scale_x);
    let height = content_h.div_ceil(scale_y);

    let mut occupied = HashSet::new();
    for row in rows {
        let (x0, x1) = scale_span(row.x, row.w, scale_x, width);
        let (y0, y1) = scale_span(row.y, row.h, scale_y, height);
        for y in y0..=y1 {
            for x in x0..=x1 {
                occupied.insert((x, y));
            }
        }
    }

    let focused = focused.map(|rect| {
        let cx = rect.x + rect.w / 2;
        let cy = rect.y + rect.h / 2;
        (
            (cx / scale_x).min(width - 1),
            (cy / scale_y).min(height - 1),
        )
    });

    let (vx0, vx1) = scale_span(viewport.x, viewport.w, scale_x, width);
    let (vy0, vy1) = scale_span(viewport.y, viewport.h, scale_y, height);
    let viewport = Rect {
        x: vx0,
        y: vy0,
        w: vx1 - vx0 + 1,
        h: vy1 - vy0 + 1,
    };

    Some(Minimap {
        width,
        height,
        occupied,
        focused,
        viewport,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect { x, y, w, h }
    }

    /// The canonical oversized fixture used across these tests: 100x30
    /// content into a 24x10 budget scales by (5, 3) into a 20x10 interior.
    fn oversized(rows: &[Rect], focused: Option<Rect>, viewport: Rect) -> Option<Minimap> {
        build(100, 30, rows, focused, viewport, 24, 10)
    }

    #[test]
    fn content_that_fits_the_viewport_yields_no_minimap() {
        assert_eq!(build(20, 10, &[], None, rect(0, 0, 80, 40), 24, 10), None);
    }

    #[test]
    fn empty_content_yields_no_minimap() {
        assert_eq!(build(0, 0, &[], None, rect(0, 0, 80, 40), 24, 10), None);
    }

    #[test]
    fn horizontal_overflow_alone_is_enough_to_show_the_map() {
        let map = build(100, 10, &[], None, rect(0, 0, 80, 40), 24, 10);
        assert!(map.is_some());
    }

    #[test]
    fn vertical_overflow_alone_is_enough_to_show_the_map() {
        let map = build(30, 100, &[], None, rect(0, 0, 80, 40), 24, 10);
        assert!(map.is_some());
    }

    #[test]
    fn each_axis_scales_by_its_own_ceiling_division() {
        let map = oversized(&[], None, rect(0, 0, 80, 20)).unwrap();
        // ceil(100 / 24) = 5, so the interior shrinks to ceil(100 / 5) = 20;
        // ceil(30 / 10) = 3 fills the height budget exactly.
        assert_eq!((map.width, map.height), (20, 10));
    }

    #[test]
    fn content_barely_over_the_viewport_gets_a_small_map_not_a_padded_one() {
        let map = build(30, 12, &[], None, rect(0, 0, 25, 10), 24, 10).unwrap();
        // ceil(30 / 24) = 2 and ceil(12 / 10) = 2: the interior is 15x6,
        // not the full 24x10 budget.
        assert_eq!((map.width, map.height), (15, 6));
    }

    #[test]
    fn row_rects_mark_the_map_cells_they_cover() {
        let rows = [rect(10, 6, 5, 1), rect(0, 0, 12, 1)];
        let map = oversized(&rows, None, rect(0, 0, 80, 20)).unwrap();
        // (10, 6, 5, 1) at scale (5, 3): columns 10..=14 all land in map
        // column 2, row 6 lands in map row 2. (0, 0, 12, 1) spans map
        // columns 0..=2 in map row 0.
        let expected: HashSet<(usize, usize)> =
            [(2, 2), (0, 0), (1, 0), (2, 0)].into_iter().collect();
        assert_eq!(map.occupied, expected);
    }

    #[test]
    fn focused_rect_maps_to_its_center_cell() {
        let focus = rect(40, 12, 20, 1);
        let map = oversized(&[focus], Some(focus), rect(0, 0, 80, 20)).unwrap();
        // Center (50, 12) at scale (5, 3) is map cell (10, 4).
        assert_eq!(map.focused, Some((10, 4)));
    }

    #[test]
    fn missing_focus_rect_leaves_no_focused_cell() {
        let map = oversized(&[], None, rect(0, 0, 80, 20)).unwrap();
        assert_eq!(map.focused, None);
    }

    #[test]
    fn viewport_maps_into_cell_space_as_an_inclusive_cell_range() {
        let map = oversized(&[], None, rect(50, 15, 40, 20)).unwrap();
        // Columns 50..=89 at scale 5 are map columns 10..=17; rows 15..=34
        // at scale 3 are map rows 5..=11, clamped to the 10-row interior.
        assert_eq!(map.viewport, rect(10, 5, 8, 5));
    }

    #[test]
    fn viewport_larger_than_content_clamps_to_the_map_bounds() {
        // Content only overflows vertically; the viewport is wider than
        // the whole content, so its mapped rect must clamp to the interior.
        let map = build(30, 100, &[], None, rect(0, 0, 80, 40), 24, 10).unwrap();
        assert_eq!((map.width, map.height), (15, 10));
        assert_eq!(map.viewport, rect(0, 0, 15, 4));
    }

    #[test]
    fn viewport_outline_is_the_rects_perimeter_ring() {
        let map = Minimap {
            width: 10,
            height: 6,
            occupied: HashSet::new(),
            focused: None,
            viewport: rect(1, 1, 3, 3),
        };
        for (x, y) in [
            (1, 1),
            (2, 1),
            (3, 1),
            (1, 2),
            (3, 2),
            (1, 3),
            (2, 3),
            (3, 3),
        ] {
            assert!(
                map.on_viewport_outline(x, y),
                "expected outline at ({x}, {y})"
            );
        }
        assert!(!map.on_viewport_outline(2, 2), "interior is not outline");
        assert!(!map.on_viewport_outline(0, 0), "outside is not outline");
        assert!(!map.on_viewport_outline(4, 2), "outside is not outline");
    }

    #[test]
    fn single_cell_viewport_is_all_outline() {
        let map = Minimap {
            width: 10,
            height: 6,
            occupied: HashSet::new(),
            focused: None,
            viewport: rect(2, 2, 1, 1),
        };
        assert!(map.on_viewport_outline(2, 2));
    }
}
