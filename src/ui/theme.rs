//! Color and stroke constants for the graph view. Dark-theme-friendly,
//! muted tones so status colors read clearly against the canvas background
//! without competing with each other.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use egui::{Color32, Stroke};

use crate::graph::model::GitStatus;

// `abbreviated_label` is a pure string function with no egui dependency --
// it lives in `crate::graph::labels` so `crate::graph::layout` can use it
// too (label-fit box sizing), and is re-exported here so existing
// `theme::abbreviated_label` call sites keep working.
pub use crate::graph::labels::abbreviated_label;

/// The central panel's background.
pub const CANVAS_BG: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x1e);

/// Base hue for dependency edges not touching the focused node -- see
/// [`edge_stroke_dim`], which dims it to ~25% alpha.
pub const EDGE_COLOR: Color32 = Color32::from_rgb(0x66, 0x66, 0x66);

/// Bright accent used for the focus ring, chosen to stand out against every
/// status color below.
pub const FOCUS_RING: Color32 = Color32::from_rgb(0x4d, 0xe8, 0xe0);

/// Leaf node fill color for `status`.
pub fn leaf_fill(status: GitStatus) -> Color32 {
    match status {
        GitStatus::Unchanged => Color32::from_rgb(0x3a, 0x3a, 0x3a),
        GitStatus::Added => Color32::from_rgb(0x2e, 0x5c, 0x2e),
        GitStatus::Modified => Color32::from_rgb(0x6b, 0x5c, 0x1f),
        GitStatus::Deleted => Color32::from_rgb(0x5c, 0x2a, 0x2a),
    }
}

/// Leaf node border color for `status` -- a lighter tint of [`leaf_fill`].
pub fn leaf_border(status: GitStatus) -> Color32 {
    match status {
        GitStatus::Unchanged => Color32::from_rgb(0x77, 0x77, 0x77),
        GitStatus::Added => Color32::from_rgb(0x6a, 0xc9, 0x6a),
        GitStatus::Modified => Color32::from_rgb(0xd9, 0xbb, 0x4a),
        GitStatus::Deleted => Color32::from_rgb(0xd9, 0x6a, 0x6a),
    }
}

/// Leaf box border stroke.
pub fn leaf_border_stroke(status: GitStatus) -> Stroke {
    Stroke::new(1.0, leaf_border(status))
}

/// Stroke for edges that don't touch the focused node: same hue as
/// [`edge_stroke`], but faint (~25% alpha) so the hairball recedes and the
/// focused node's own edges (see [`edge_stroke_outgoing`]/
/// [`edge_stroke_incoming`]) read as the story.
pub fn edge_stroke_dim() -> Stroke {
    let [r, g, b, _] = EDGE_COLOR.to_array();
    Stroke::new(1.0, Color32::from_rgba_unmultiplied(r, g, b, 64))
}

/// Warm accent for edges leaving the focused node (it depends on the
/// target).
pub const EDGE_OUTGOING: Color32 = Color32::from_rgb(0xe0, 0x8a, 0x3d);

/// Cool accent for edges arriving at the focused node (the source depends
/// on it).
pub const EDGE_INCOMING: Color32 = Color32::from_rgb(0x4d, 0xc8, 0xe8);

/// Stroke for edges out of the focused node -- "focused depends on".
pub fn edge_stroke_outgoing() -> Stroke {
    Stroke::new(2.0, EDGE_OUTGOING)
}

/// Stroke for edges into the focused node -- "depends on focused".
pub fn edge_stroke_incoming() -> Stroke {
    Stroke::new(2.0, EDGE_INCOMING)
}

/// Color of the small "tested" badge glyph drawn on a node that has a
/// changed test module covering it (see
/// [`crate::graph::test_modules::nodes_with_changed_tests`]). A muted green,
/// distinct from [`leaf_fill`]'s `Added` green so it doesn't read as a
/// status change.
pub const TESTED_BADGE_COLOR: Color32 = Color32::from_rgb(0x6a, 0xc9, 0x6a);

/// Faint horizontal line color separating one layer's band from the next.
pub const BAND_SEPARATOR: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2c);

/// Stroke used to paint the horizontal band separators between layers.
pub fn band_separator_stroke() -> Stroke {
    Stroke::new(1.0, BAND_SEPARATOR)
}

/// Stroke used to paint the focus ring around the focused node's rect.
pub fn focus_ring_stroke() -> Stroke {
    Stroke::new(2.0, FOCUS_RING)
}

/// Border stroke for the file viewer pane: [`FOCUS_RING`] when it has
/// keyboard focus ([`crate::core::app::Pane::File`]), a faint gray
/// otherwise -- the graph pane signals its own focus via the focused
/// node's ring, so the file pane only needs to visibly dim, not vanish,
/// when focus is elsewhere.
pub fn pane_border_stroke(focused: bool) -> Stroke {
    if focused {
        Stroke::new(2.0, FOCUS_RING)
    } else {
        Stroke::new(1.0, Color32::from_rgb(0x3a, 0x3a, 0x3a))
    }
}

/// A deterministic, saturated hue for `root_id`, used as a node's left-edge
/// stripe and its legend-row swatch (see [`crate::ui::graph_view`]) so a
/// namespace root reads as "the same color everywhere" without vdiff having
/// to track or persist a color assignment across runs -- the same root id
/// always hashes to the same color, on this run and the next.
pub fn root_hue_color(root_id: &str) -> Color32 {
    let mut hasher = DefaultHasher::new();
    root_id.hash(&mut hasher);
    let hash = hasher.finish();

    // Golden-angle-ish spread over hue so adjacent hashes don't cluster on
    // similar hues; fixed, fairly high saturation/value tuned to stay
    // legible against the dark canvas background.
    let hue = (hash % 360) as f32;
    hsv_to_rgb(hue, 0.55, 0.85)
}

/// Minimal HSV -> RGB conversion (h in degrees `[0, 360)`, s/v in `[0, 1]`).
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Color32 {
    let c = v * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match h_prime as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    Color32::from_rgb(
        ((r1 + m) * 255.0).round() as u8,
        ((g1 + m) * 255.0).round() as u8,
        ((b1 + m) * 255.0).round() as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_hue_color_is_deterministic() {
        assert_eq!(root_hue_color("App.Leads"), root_hue_color("App.Leads"));
    }

    #[test]
    fn root_hue_color_differs_across_distinct_roots() {
        // Not a strict guarantee for arbitrary strings (hash collisions on
        // hue exist), but true for typical distinct root names -- catches a
        // constant-color regression.
        assert_ne!(root_hue_color("App.Leads"), root_hue_color("App.Billing"));
    }
}
