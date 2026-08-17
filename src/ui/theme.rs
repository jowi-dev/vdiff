//! Color and stroke constants for the graph view. Dark-theme-friendly,
//! muted tones so status colors read clearly against the canvas background
//! without competing with each other.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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

/// Abbreviate `id`'s qualified name for display by stripping its top-level
/// root's own qualified prefix: `elixir:App.Leads.Lead` under root
/// `elixir:App` becomes `Leads.Lead`. `id` naming `root_id` itself (the
/// root's own module) shows `display_name` plain, matching every other
/// node's own name. Falls back to `id`'s body (language prefix stripped)
/// if it doesn't actually start with the root's prefix (shouldn't happen
/// for a well-formed graph, but avoids stripping the wrong thing).
pub fn abbreviated_label(id: &str, root_id: &str, display_name: &str) -> String {
    let id_body = strip_lang_prefix(id);
    let root_body = strip_lang_prefix(root_id);

    if id_body == root_body {
        return display_name.to_string();
    }

    let sep = lang_separator(id);
    let prefix = format!("{root_body}{sep}");
    match id_body.strip_prefix(&prefix) {
        Some(rest) => rest.to_string(),
        None => id_body.to_string(),
    }
}

/// Strip a `lang:` namespace prefix (`elixir:`, `rust:`, `file:` -- see
/// [`crate::graph::builder`]) off an id string, if present.
fn strip_lang_prefix(id: &str) -> &str {
    match id.find(':') {
        Some(idx) => &id[idx + 1..],
        None => id,
    }
}

/// The path separator a given id's language namespace uses between
/// segments, so [`abbreviated_label`] strips exactly the root prefix and
/// not part of the next segment's name.
fn lang_separator(id: &str) -> &'static str {
    if id.starts_with("rust:") {
        "::"
    } else if id.starts_with("file:") {
        "/"
    } else {
        // elixir: and anything unrecognized.
        "."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviated_label_strips_elixir_root_prefix() {
        assert_eq!(
            abbreviated_label("elixir:App.Leads.Lead", "elixir:App", "Lead"),
            "Leads.Lead"
        );
    }

    #[test]
    fn abbreviated_label_shows_plain_name_for_the_root_itself() {
        assert_eq!(abbreviated_label("elixir:App", "elixir:App", "App"), "App");
    }

    #[test]
    fn abbreviated_label_strips_rust_root_prefix() {
        assert_eq!(
            abbreviated_label("rust:crate_a::foo::bar", "rust:crate_a", "bar"),
            "foo::bar"
        );
    }

    #[test]
    fn abbreviated_label_falls_back_to_full_body_when_not_prefixed() {
        assert_eq!(
            abbreviated_label("elixir:Other.Thing", "elixir:App", "Thing"),
            "Other.Thing"
        );
    }

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
