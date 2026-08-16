//! Color and stroke constants for the graph view. Dark-theme-friendly,
//! muted tones so status colors read clearly against the canvas background
//! without competing with each other.

use egui::{Color32, Stroke};

use crate::graph::model::GitStatus;

/// The central panel's background.
pub const CANVAS_BG: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x1e);

/// Thin lines connecting dependency edges.
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

/// Container (non-leaf) box fill: translucent, so nested contents stay
/// legible under it.
pub fn container_fill(status: GitStatus) -> Color32 {
    let c = leaf_fill(status);
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 0x50)
}

/// Container box border stroke.
pub fn container_border(status: GitStatus) -> Stroke {
    Stroke::new(1.5, leaf_border(status))
}

/// Leaf box border stroke.
pub fn leaf_border_stroke(status: GitStatus) -> Stroke {
    Stroke::new(1.0, leaf_border(status))
}

/// Stroke used to paint dependency edges.
pub fn edge_stroke() -> Stroke {
    Stroke::new(1.0, EDGE_COLOR)
}

/// Stroke used to paint the focus ring around the focused node's rect.
pub fn focus_ring_stroke() -> Stroke {
    Stroke::new(2.0, FOCUS_RING)
}
