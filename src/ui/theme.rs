//! Color and stroke constants for the graph view. Dark-theme-friendly,
//! muted tones so status colors read clearly against the canvas background
//! without competing with each other.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use egui::{Color32, Stroke};

use crate::graph::model::GitStatus;
use crate::review::findings::Severity;

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

/// Desaturate `color` toward its own gray (average-channel luminance),
/// keeping alpha untouched -- the fill treatment for a reviewed node (see
/// [`crate::ui::graph_view::paint_node`]'s doc): the status color is still
/// legible (it's not fully grayed out), just visibly muted next to an
/// unreviewed sibling of the same status. Blends 1/3 original color, 2/3
/// gray; deliberately not full grayscale, so added/modified/deleted still
/// read apart from each other even once reviewed.
pub fn dim_reviewed(color: Color32) -> Color32 {
    let [r, g, b, a] = color.to_array();
    let gray = ((r as u32 + g as u32 + b as u32) / 3) as u8;
    let mix = |c: u8| ((c as u16 + 2 * gray as u16) / 3) as u8;
    Color32::from_rgba_unmultiplied(mix(r), mix(g), mix(b), a)
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

/// Color for a findings badge (see [`crate::review::findings::badge`]) of
/// `severity` -- red `high`, orange `medium`, yellow `low`, matching the
/// severity's own urgency the way [`leaf_fill`]'s status colors already
/// map git status to hue. Distinct from every [`leaf_fill`]/
/// [`TESTED_BADGE_COLOR`] hue so a findings badge never gets mistaken for
/// either.
pub fn severity_color(severity: Severity) -> Color32 {
    match severity {
        Severity::High => Color32::from_rgb(0xe0, 0x4a, 0x4a),
        Severity::Medium => Color32::from_rgb(0xe0, 0x8a, 0x3d),
        Severity::Low => Color32::from_rgb(0xd9, 0xc9, 0x3d),
    }
}

/// Screen-space breathing room kept above the topmost node when the graph
/// first opens (baked into [`crate::ui::graph_view::Transform`]'s default
/// offset) and preserved by auto-pan (see
/// [`crate::ui::graph_view::clamp_into_view`]) -- so layer 0 never sits
/// flush against the window's top edge. Now that the legend has moved to a
/// screen-anchored corner (see [`crate::ui::graph_view::paint_legend`]),
/// this is purely visual comfort, not collision avoidance.
pub const GRAPH_TOP_PADDING: f32 = 24.0;

/// Background chip color for screen-anchored overlays painted on top of the
/// graph -- the legend and the focused-node status readout -- so their text
/// stays legible over whatever edges/nodes happen to sit underneath.
pub fn overlay_chip_bg() -> Color32 {
    Color32::from_rgba_unmultiplied(0x1e, 0x1e, 0x1e, 200)
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

/// Alpha (`0..=255`) applied to every *background* the fullscreen editor
/// overlay ([`crate::ui::overlay`]) paints over the already-drawn graph --
/// the nvim grid's per-cell backgrounds ([`crate::ui::nvim_pane`]) and the
/// built-in file viewer's panel wash alike -- so the editor reads as one
/// ~85% opaque surface with the graph showing through as a faint ambient
/// glow. Text glyphs and the cursor block are painted fully opaque
/// regardless (see [`crate::ui::nvim_pane::show`]) -- only backgrounds get
/// this alpha, since legibility of the text itself is non-negotiable.
///
/// There is deliberately only **one** alpha constant in this story. An
/// earlier revision painted a separate full-viewport scrim *underneath* the
/// editor content's own (fully opaque) backgrounds; lowering the scrim's
/// alpha did nothing observable because the opaque content painted on top
/// of it still covered 100% of the pixels. This revision removes that
/// scrim entirely -- the editor's content backgrounds (translucent via
/// [`translucent`]) are themselves what let the graph show through, so
/// there is nothing left to stack multiplicatively into double-dimming.
/// The one thing that stays fully opaque is the header strip
/// ([`OVERLAY_HEADER_BG`]) -- solid UI chrome, not part of the
/// "graph shows through" surface.
pub const EDITOR_BG_ALPHA: u8 = 217;

/// `bg` with its alpha channel replaced by [`EDITOR_BG_ALPHA`], rgb
/// untouched. The one function every translucent-background paint call in
/// the editor overlay goes through (nvim cell backgrounds, the built-in
/// viewer's panel wash) -- see [`EDITOR_BG_ALPHA`]'s doc for why there is
/// only one alpha in this whole story. For a reverse-video cell, callers
/// swap fg/bg *before* calling this (see
/// [`crate::ui::nvim_pane::colors_for`]) -- this function only ever adjusts
/// alpha, never decides which color is "the background".
pub fn translucent(bg: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(bg.r(), bg.g(), bg.b(), EDITOR_BG_ALPHA)
}

/// The overlay's header strip background -- fully opaque (unlike the
/// scrim beneath it) and a shade lighter than [`CANVAS_BG`] so the strip
/// reads as a distinct, solid surface rather than more of the dimmed
/// graph showing through.
pub const OVERLAY_HEADER_BG: Color32 = Color32::from_rgb(0x28, 0x28, 0x28);

/// The overlay header strip's text color.
pub const OVERLAY_HEADER_TEXT: Color32 = Color32::from_rgb(0xe0, 0xe0, 0xe0);

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
    fn dim_reviewed_leaves_alpha_untouched() {
        let color = Color32::from_rgba_unmultiplied(0x6b, 0x5c, 0x1f, 0xaa);
        assert_eq!(dim_reviewed(color).a(), 0xaa);
    }

    #[test]
    fn dim_reviewed_moves_every_channel_toward_gray_without_flattening() {
        // Modified's fill is (0x6b, 0x5c, 0x1f): r and g sit above their
        // shared average, b sits below it -- dimming should pull r/g down
        // and b up, landing strictly between the original and full gray,
        // never collapsing all three channels to one value.
        let modified = leaf_fill(GitStatus::Modified);
        let dimmed = dim_reviewed(modified);
        let [r, g, b, _] = modified.to_array();
        let [dr, dg, db, _] = dimmed.to_array();
        assert!(dr < r, "r should move down toward gray: {dr} vs {r}");
        assert!(dg < g, "g should move down toward gray: {dg} vs {g}");
        assert!(db > b, "b should move up toward gray: {db} vs {b}");
        assert_ne!(
            dr, dg,
            "channels shouldn't fully collapse to a single gray value"
        );
    }

    #[test]
    fn dim_reviewed_is_a_noop_on_true_gray() {
        let gray = Color32::from_rgb(0x40, 0x40, 0x40);
        assert_eq!(dim_reviewed(gray), gray);
    }

    #[test]
    fn translucent_sets_editor_bg_alpha() {
        // `Color32` stores premultiplied alpha internally (see
        // `ecolor::Color32::from_rgba_unmultiplied`), so the rgb channels
        // aren't preserved bit-for-bit after this call -- only the alpha
        // channel and "same input always converts the same way" are
        // observable guarantees at this level; egui un-premultiplies for
        // painting, per the module's sanity note.
        let bg = Color32::from_rgb(0x12, 0x34, 0x56);
        let result = translucent(bg);
        assert_eq!(result.a(), EDITOR_BG_ALPHA);
        assert_eq!(
            result,
            Color32::from_rgba_unmultiplied(0x12, 0x34, 0x56, EDITOR_BG_ALPHA)
        );
    }

    #[test]
    fn translucent_on_reverse_video_swap_uses_the_swapped_color() {
        // Reverse video swaps fg/bg *before* translucent() ever sees it --
        // this just asserts translucent() itself doesn't care which color
        // it's handed, only that it stamps the alpha on whatever it's given.
        let original_fg = Color32::from_rgb(0xaa, 0xbb, 0xcc);
        let swapped_bg = original_fg; // caller already did the swap
        let result = translucent(swapped_bg);
        assert_eq!(result.a(), EDITOR_BG_ALPHA);
        assert_eq!(
            result,
            Color32::from_rgba_unmultiplied(0xaa, 0xbb, 0xcc, EDITOR_BG_ALPHA)
        );
    }

    #[test]
    fn translucent_differs_for_differing_backgrounds() {
        let a = translucent(Color32::from_rgb(0x10, 0x10, 0x10));
        let b = translucent(Color32::from_rgb(0x20, 0x20, 0x20));
        assert_ne!(a, b);
    }

    #[test]
    fn root_hue_color_differs_across_distinct_roots() {
        // Not a strict guarantee for arbitrary strings (hash collisions on
        // hue exist), but true for typical distinct root names -- catches a
        // constant-color regression.
        assert_ne!(root_hue_color("App.Leads"), root_hue_color("App.Billing"));
    }
}
