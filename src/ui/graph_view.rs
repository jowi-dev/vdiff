//! Paints the layered-dependency graph on a central panel with
//! [`egui::Painter`] directly (rects, labels, straight edge lines) rather
//! than a node-graph widget. Namespace containment is no longer drawn as
//! nested boxes (see [`crate::graph::layers`]/[`crate::graph::layout`]):
//! every node is a flat, leaf-sized box positioned by dependency layer, with
//! a colored left-edge stripe conveying which top-level root it belongs to
//! and an abbreviated qualified label. A small legend row pinned at the top
//! of the canvas maps each root's name to its hue.
//!
//! Pan/zoom is view-only state ([`Transform`]) that lives in the eframe
//! glue, never in [`crate::core::app::App`] -- the core stays geometry-free.

use std::collections::HashSet;

use egui::{Align2, Color32, FontId, Pos2, Rect as EguiRect, Sense, StrokeKind, Ui, Vec2};

use crate::core::app::App;
use crate::graph::layout::{self, LayoutResult, Pos as LPos, Rect as LRect};
use crate::graph::model::{GitStatus, NodeId, ProjectGraph};
use crate::graph::test_modules::{hide_test_modules, nodes_with_changed_tests};
use crate::ui::theme;

/// Zoom lower bound (10%).
pub const MIN_SCALE: f32 = 0.1;
/// Zoom upper bound (500%).
pub const MAX_SCALE: f32 = 5.0;

/// Width of a node's colored left-edge stripe conveying its top-level root
/// (screen-space pixels, independent of zoom -- a thin stripe stays legible
/// at every scale rather than shrinking away when zoomed out).
const STRIPE_W: f32 = 4.0;

/// Height reserved for the legend row pinned at the top of the canvas.
const LEGEND_H: f32 = 22.0;

/// Pan/zoom applied to every layout-space point at paint time: `screen =
/// layout * scale + offset`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub scale: f32,
    pub offset: Vec2,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            scale: 1.0,
            offset: Vec2::ZERO,
        }
    }
}

impl Transform {
    /// Map a layout-space point to screen space.
    pub fn to_screen_pos(&self, p: LPos) -> Pos2 {
        Pos2::new(p.x * self.scale, p.y * self.scale) + self.offset
    }

    /// Map a layout-space rect to screen space.
    pub fn to_screen_rect(&self, r: LRect) -> EguiRect {
        let min = self.to_screen_pos(r.origin);
        let size = Vec2::new(r.size.w * self.scale, r.size.h * self.scale);
        EguiRect::from_min_size(min, size)
    }

    /// Translate the view by `delta` (screen-space pixels).
    pub fn pan(&mut self, delta: Vec2) {
        self.offset += delta;
    }

    /// Zoom by `factor` (>1 zooms in, <1 zooms out), clamped to
    /// [`MIN_SCALE`]/[`MAX_SCALE`], keeping the layout-space point currently
    /// under `around_screen` stationary on screen.
    pub fn zoom(&mut self, factor: f32, around_screen: Pos2) {
        let new_scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        let ratio = new_scale / self.scale;
        self.offset = around_screen.to_vec2() - (around_screen.to_vec2() - self.offset) * ratio;
        self.scale = new_scale;
    }
}

/// Paint the graph into `ui`'s available space: background, edges, node
/// rects, then the pinned legend row on top. Handles pan (drag) and zoom
/// (scroll) on the empty canvas, and auto-pans so a newly focused node comes
/// into view.
///
/// `last_focus` is view-only state (owned by the eframe glue, not
/// [`crate::core::app::App`]) that remembers which node the auto-pan last
/// ran for. The auto-pan only fires when `app.focus` differs from it --
/// running `clamp_into_view` unconditionally on every repaint (including
/// the continuous repaints egui schedules while the pointer merely moves
/// over the canvas) fed the transform's own screen-space output back into
/// itself every frame; sub-pixel rounding in that round trip kept nudging
/// `transform.offset` back and forth instead of settling at zero, which
/// painted as flicker on the node rects. Gating on a real focus change
/// makes the pan run once per focus move and leaves the transform alone
/// otherwise.
pub fn show(
    ui: &mut Ui,
    app: &App,
    layout: &LayoutResult,
    transform: &mut Transform,
    last_focus: &mut Option<NodeId>,
) {
    let viewport = ui.max_rect();
    let response = ui.allocate_rect(viewport, Sense::click_and_drag());

    handle_pan_zoom(ui, &response, transform);

    if last_focus.as_ref() != Some(&app.focus) {
        if let Some(focus_rect) = layout.rects.get(&app.focus) {
            let screen_rect = transform.to_screen_rect(*focus_rect);
            transform.pan(clamp_into_view(screen_rect, response.rect));
        }
        *last_focus = Some(app.focus.clone());
    }

    let painter = ui.painter_at(response.rect);
    painter.rect_filled(response.rect, 0.0, theme::CANVAS_BG);

    let tested = nodes_with_changed_tests(&app.graph);

    paint_band_separators(&painter, layout, transform, response.rect);
    paint_edges(&painter, layout, transform, &app.focus);
    for layer in &layout.layers {
        for id in layer {
            paint_node(
                &painter, &app.graph, layout, transform, id, &app.focus, &tested,
            );
        }
    }

    paint_legend(&painter, app, layout, response.rect);
}

/// Drag pans the view; scroll zooms around the pointer position.
fn handle_pan_zoom(ui: &Ui, response: &egui::Response, transform: &mut Transform) {
    if response.dragged() {
        transform.pan(response.drag_delta());
    }
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let factor = (scroll * 0.002).exp();
            let around = response
                .hover_pos()
                .unwrap_or_else(|| response.rect.center());
            transform.zoom(factor, around);
        }
    }
}

/// The offset delta (screen-space) needed to bring `focus_rect` fully
/// inside `viewport`, with no animation -- zero if it's already visible.
/// Pure function of plain rect data, unit-tested below.
pub fn clamp_into_view(focus_rect: EguiRect, viewport: EguiRect) -> Vec2 {
    let dx = if focus_rect.left() < viewport.left() {
        viewport.left() - focus_rect.left()
    } else if focus_rect.right() > viewport.right() {
        viewport.right() - focus_rect.right()
    } else {
        0.0
    };
    let dy = if focus_rect.top() < viewport.top() {
        viewport.top() - focus_rect.top()
    } else if focus_rect.bottom() > viewport.bottom() {
        viewport.bottom() - focus_rect.bottom()
    } else {
        0.0
    };
    Vec2::new(dx, dy)
}

/// Paint every edge, partitioned by how it relates to `focus`: edges
/// untouched by focus paint first, faint (see [`theme::edge_stroke_dim`]) so
/// the hairball recedes; then the focused node's outgoing edges (it depends
/// on the target) in a warm accent; then its incoming edges (the source
/// depends on it) in a cool accent -- both drawn on top, full alpha, so the
/// focused node's connections read clearly against the rest.
fn paint_edges(
    painter: &egui::Painter,
    layout: &LayoutResult,
    transform: &Transform,
    focus: &NodeId,
) {
    let mut rest = Vec::new();
    let mut outgoing = Vec::new();
    let mut incoming = Vec::new();
    for edge in &layout.edges {
        if &edge.from == focus {
            outgoing.push(edge);
        } else if &edge.to == focus {
            incoming.push(edge);
        } else {
            rest.push(edge);
        }
    }

    for edge in rest {
        paint_edge(painter, edge, transform, theme::edge_stroke_dim());
    }
    for edge in outgoing {
        paint_edge(painter, edge, transform, theme::edge_stroke_outgoing());
    }
    for edge in incoming {
        paint_edge(painter, edge, transform, theme::edge_stroke_incoming());
    }
}

fn paint_edge(
    painter: &egui::Painter,
    edge: &crate::graph::layout::EdgePath,
    transform: &Transform,
    stroke: egui::Stroke,
) {
    let p0 = transform.to_screen_pos(edge.points[0]);
    let p1 = transform.to_screen_pos(edge.points[1]);
    painter.line_segment([p0, p1], stroke);
}

/// A faint horizontal line between each pair of adjacent layers, spanning
/// the viewport width, at the midpoint between one layer's lowest rect
/// bottom and the next layer's highest rect top. Skips a boundary if either
/// layer is empty (shouldn't happen, but `layout.layers` is caller data).
fn paint_band_separators(
    painter: &egui::Painter,
    layout: &LayoutResult,
    transform: &Transform,
    viewport: EguiRect,
) {
    for pair in layout.layers.windows(2) {
        let (above, below) = (&pair[0], &pair[1]);
        let Some(above_bottom) = layer_extent(above, layout).map(|(_, bottom)| bottom) else {
            continue;
        };
        let Some(below_top) = layer_extent(below, layout).map(|(top, _)| top) else {
            continue;
        };
        let mid_y = (above_bottom + below_top) / 2.0;
        let screen_y = transform.to_screen_pos(LPos { x: 0.0, y: mid_y }).y;
        painter.line_segment(
            [
                Pos2::new(viewport.left(), screen_y),
                Pos2::new(viewport.right(), screen_y),
            ],
            theme::band_separator_stroke(),
        );
    }
}

/// The `(min top, max bottom)` layout-space y-extent of `layer`'s rects, or
/// `None` if none of its ids have a rect.
fn layer_extent(layer: &[NodeId], layout: &LayoutResult) -> Option<(f32, f32)> {
    layer.iter().filter_map(|id| layout.rects.get(id)).fold(
        None,
        |acc: Option<(f32, f32)>, rect| {
            let top = rect.origin.y;
            let bottom = rect.origin.y + rect.size.h;
            Some(match acc {
                Some((min_top, max_bottom)) => (min_top.min(top), max_bottom.max(bottom)),
                None => (top, bottom),
            })
        },
    )
}

/// Screen-space padding kept clear of each side of a node's label, on top
/// of the root-hue stripe -- used both to decide when the label needs
/// truncating and to inset the text draw itself.
const LABEL_PAD: f32 = 6.0;

/// Paint `id`'s rect: status fill/border, a left-edge stripe in its
/// top-level root's hue, a truncated-to-fit abbreviated label, and a small
/// green "tested" badge if `tested` flags it. Draws the focus ring on top
/// if `id` is `focus`.
fn paint_node(
    painter: &egui::Painter,
    graph: &ProjectGraph,
    layout: &LayoutResult,
    transform: &Transform,
    id: &NodeId,
    focus: &NodeId,
    tested: &HashSet<NodeId>,
) {
    let Some(node) = graph.node(id) else {
        return;
    };
    let Some(rect) = layout.rects.get(id) else {
        return;
    };
    let screen_rect = transform.to_screen_rect(*rect);

    painter.rect(
        screen_rect,
        2.0,
        theme::leaf_fill(node.status),
        theme::leaf_border_stroke(node.status),
        StrokeKind::Inside,
    );

    let root_id = graph.top_level_root(id);
    let stripe_color = theme::root_hue_color(&root_id.to_string());
    let stripe_rect =
        EguiRect::from_min_size(screen_rect.min, Vec2::new(STRIPE_W, screen_rect.height()));
    painter.rect_filled(stripe_rect, 0.0, stripe_color);

    let label = theme::abbreviated_label(&id.to_string(), &root_id.to_string(), &node.display_name);
    let font_size = 12.0 * transform.scale.max(0.3);
    let available_px = screen_rect.width() - STRIPE_W - 2.0 * LABEL_PAD;
    let label = fit_label(&label, available_px, font_size);
    painter.text(
        screen_rect.center(),
        Align2::CENTER_CENTER,
        &label,
        FontId::proportional(font_size),
        label_color(node.status),
    );

    if tested.contains(id) {
        painter.text(
            Pos2::new(
                screen_rect.right() - LABEL_PAD,
                screen_rect.top() + LABEL_PAD,
            ),
            Align2::RIGHT_TOP,
            "\u{2713}",
            FontId::proportional((10.0 * transform.scale.max(0.3)).max(6.0)),
            theme::TESTED_BADGE_COLOR,
        );
    }

    if id == focus {
        painter.rect_stroke(
            screen_rect,
            2.0,
            theme::focus_ring_stroke(),
            StrokeKind::Inside,
        );
    }
}

/// Truncate `label` with a trailing `…` if it wouldn't fit in `available_px`
/// at `font_size`, estimating character width from
/// [`layout::CHAR_W`] scaled to `font_size` (that constant is calibrated at
/// a 12px font) -- the same char-count estimate [`crate::graph::layout`]
/// uses to size the box in the first place, so the two stay consistent even
/// though this is a cheaper approximation than measuring a real galley.
fn fit_label(label: &str, available_px: f32, font_size: f32) -> String {
    let char_w_px = layout::CHAR_W * (font_size / 12.0);
    if char_w_px <= 0.0 {
        return label.to_string();
    }
    let max_chars = (available_px / char_w_px).floor().max(1.0) as usize;
    if label.chars().count() <= max_chars {
        return label.to_string();
    }
    let keep = max_chars.saturating_sub(1).max(1);
    let truncated: String = label.chars().take(keep).collect();
    format!("{truncated}…")
}

/// The pinned legend, two screen-space rows: root-hue swatches (unchanged),
/// then a hint row -- the hidden/shown test-module count with the `t` key
/// reminder, and two edge-color swatches explaining
/// [`theme::edge_stroke_outgoing`]/[`theme::edge_stroke_incoming`].
fn paint_legend(painter: &egui::Painter, app: &App, layout: &LayoutResult, viewport: EguiRect) {
    paint_root_legend(painter, &app.graph, layout, viewport);
    paint_hint_row(painter, app, viewport);
}

/// Row 1: every distinct top-level root's name in its
/// [`theme::root_hue_color`], root ids sorted for a stable left-to-right
/// order across frames.
fn paint_root_legend(
    painter: &egui::Painter,
    graph: &ProjectGraph,
    layout: &LayoutResult,
    viewport: EguiRect,
) {
    let mut roots: Vec<NodeId> = layout
        .layers
        .iter()
        .flatten()
        .map(|id| graph.top_level_root(id))
        .collect();
    roots.sort();
    roots.dedup();

    let mut cursor_x = viewport.left() + 8.0;
    let text_y = viewport.top() + LEGEND_H / 2.0;
    for root_id in roots {
        let name = graph
            .node(&root_id)
            .map(|n| n.display_name.clone())
            .unwrap_or_else(|| root_id.to_string());
        let color = theme::root_hue_color(&root_id.to_string());

        let swatch =
            EguiRect::from_min_size(Pos2::new(cursor_x, text_y - 5.0), Vec2::new(10.0, 10.0));
        painter.rect_filled(swatch, 2.0, color);
        cursor_x += 14.0;

        let galley = painter.layout_no_wrap(name.clone(), FontId::proportional(12.0), color);
        let text_pos = Pos2::new(cursor_x, text_y - galley.size().y / 2.0);
        painter.galley(text_pos, galley, color);
        cursor_x += 16.0 + name.len() as f32 * 6.5;
    }
}

/// Row 2: the `Enter`/`d` pane-open hint, the test-module hidden/shown hint
/// (only drawn once there are any test modules to mention at all), then the
/// two edge-color swatches.
fn paint_hint_row(painter: &egui::Painter, app: &App, viewport: EguiRect) {
    const HINT_COLOR: Color32 = Color32::from_rgb(0xaa, 0xaa, 0xaa);

    let mut cursor_x = viewport.left() + 8.0;
    let text_y = viewport.top() + LEGEND_H + LEGEND_H / 2.0;

    cursor_x = paint_text(
        painter,
        "Enter: file   d: diff",
        cursor_x,
        text_y,
        HINT_COLOR,
    ) + 20.0;

    let (_, hidden_count) = hide_test_modules(&app.graph);
    if hidden_count > 0 {
        let hint = if app.show_tests {
            format!("showing {hidden_count} test modules — t to hide")
        } else {
            format!("{hidden_count} test modules hidden — t to show")
        };
        cursor_x = paint_text(painter, &hint, cursor_x, text_y, HINT_COLOR) + 20.0;
    }

    cursor_x = paint_edge_swatch(painter, theme::EDGE_OUTGOING, "→ deps", cursor_x, text_y);
    paint_edge_swatch(
        painter,
        theme::EDGE_INCOMING,
        "← dependents",
        cursor_x,
        text_y,
    );
}

/// Draw `text` left-aligned at `(x, mid_y)` (vertically centered on
/// `mid_y`), returning the x coordinate just past its right edge.
fn paint_text(painter: &egui::Painter, text: &str, x: f32, mid_y: f32, color: Color32) -> f32 {
    let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(11.0), color);
    let width = galley.size().x;
    painter.galley(Pos2::new(x, mid_y - galley.size().y / 2.0), galley, color);
    x + width
}

/// A short line swatch in `color` followed by `label`, for the edge-color
/// hint. Returns the x coordinate just past the label, plus trailing gap.
fn paint_edge_swatch(
    painter: &egui::Painter,
    color: Color32,
    label: &str,
    x: f32,
    mid_y: f32,
) -> f32 {
    const LINE_LEN: f32 = 16.0;
    painter.line_segment(
        [Pos2::new(x, mid_y), Pos2::new(x + LINE_LEN, mid_y)],
        egui::Stroke::new(2.0, color),
    );
    paint_text(painter, label, x + LINE_LEN + 4.0, mid_y, color) + 14.0
}

/// Label text color: near-white, readable against every status fill.
fn label_color(_status: GitStatus) -> Color32 {
    Color32::from_rgb(0xea, 0xea, 0xea)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_label_leaves_a_short_label_untouched() {
        assert_eq!(fit_label("Lead", 200.0, 12.0), "Lead");
    }

    #[test]
    fn fit_label_truncates_with_an_ellipsis_when_too_wide() {
        let label = "AVeryLongModuleNameThatWontFit";
        let fitted = fit_label(label, 40.0, 12.0);
        assert!(fitted.ends_with('…'));
        assert!(fitted.len() < label.len());
    }

    #[test]
    fn fit_label_never_grows_the_string() {
        let label = "Short";
        let fitted = fit_label(label, 1.0, 12.0);
        assert!(fitted.chars().count() <= label.chars().count());
    }

    #[test]
    fn transform_identity_maps_points_unchanged() {
        let t = Transform::default();
        let p = LPos { x: 10.0, y: 20.0 };
        assert_eq!(t.to_screen_pos(p), Pos2::new(10.0, 20.0));
    }

    #[test]
    fn pan_translates_points() {
        let mut t = Transform::default();
        t.pan(Vec2::new(5.0, -3.0));
        let p = LPos { x: 10.0, y: 20.0 };
        assert_eq!(t.to_screen_pos(p), Pos2::new(15.0, 17.0));
    }

    #[test]
    fn zoom_clamps_to_bounds() {
        let mut t = Transform::default();
        t.zoom(0.0001, Pos2::new(0.0, 0.0));
        assert_eq!(t.scale, MIN_SCALE);

        let mut t = Transform::default();
        t.zoom(1000.0, Pos2::new(0.0, 0.0));
        assert_eq!(t.scale, MAX_SCALE);
    }

    #[test]
    fn zoom_keeps_point_under_cursor_stationary() {
        let mut t = Transform::default();
        let cursor = Pos2::new(50.0, 50.0);
        let layout_pt = LPos { x: 50.0, y: 50.0 };
        // The point under the cursor maps to the cursor position before...
        assert_eq!(t.to_screen_pos(layout_pt), cursor);
        t.zoom(2.0, cursor);
        // ...and after zooming.
        let after = t.to_screen_pos(layout_pt);
        assert!((after.x - cursor.x).abs() < 0.001);
        assert!((after.y - cursor.y).abs() < 0.001);
    }

    /// The general anchor-invariance property `zoom` must hold: whatever
    /// world point sits under `anchor` before the call still maps to
    /// `anchor` after, for a transform that already carries a non-identity
    /// pan and scale (the trivial default-transform case above can pass
    /// even with a wrong-coordinate-frame formula, since offset starts at
    /// zero -- this one can't).
    #[test]
    fn zoom_at_keeps_world_point_under_anchor_with_prior_pan_and_scale() {
        let mut t = Transform {
            scale: 2.0,
            offset: Vec2::new(30.0, -15.0),
        };
        let anchor = Pos2::new(120.0, 80.0);
        let world_under_anchor = LPos {
            x: (anchor.x - t.offset.x) / t.scale,
            y: (anchor.y - t.offset.y) / t.scale,
        };
        assert_eq!(t.to_screen_pos(world_under_anchor), anchor);

        t.zoom(1.6, anchor);

        let after = t.to_screen_pos(world_under_anchor);
        assert!((after.x - anchor.x).abs() < 0.001, "x drifted: {after:?}");
        assert!((after.y - anchor.y).abs() < 0.001, "y drifted: {after:?}");
    }

    #[test]
    fn clamp_into_view_is_noop_when_already_visible() {
        let viewport = EguiRect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let focus = EguiRect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(50.0, 50.0));
        assert_eq!(clamp_into_view(focus, viewport), Vec2::ZERO);
    }

    #[test]
    fn clamp_into_view_pulls_rect_left_of_viewport_back_in() {
        let viewport = EguiRect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0));
        let focus = EguiRect::from_min_size(Pos2::new(-50.0, 100.0), Vec2::new(50.0, 50.0));
        let delta = clamp_into_view(focus, viewport);
        assert_eq!(delta, Vec2::new(50.0, 0.0));
    }

    #[test]
    fn clamp_into_view_pulls_rect_right_of_viewport_back_in() {
        let viewport = EguiRect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0));
        let focus = EguiRect::from_min_size(Pos2::new(780.0, 100.0), Vec2::new(50.0, 50.0));
        let delta = clamp_into_view(focus, viewport);
        assert_eq!(delta, Vec2::new(800.0 - 830.0, 0.0));
    }

    #[test]
    fn layer_extent_covers_every_rect_in_the_layer() {
        use crate::graph::layout::Size;
        use std::collections::HashMap;

        let mut rects = HashMap::new();
        rects.insert(
            NodeId::from("a"),
            LRect {
                origin: LPos { x: 0.0, y: 10.0 },
                size: Size { w: 10.0, h: 20.0 },
            },
        );
        rects.insert(
            NodeId::from("b"),
            LRect {
                origin: LPos { x: 0.0, y: 5.0 },
                size: Size { w: 10.0, h: 20.0 },
            },
        );
        let layout = LayoutResult {
            rects,
            edges: vec![],
            layers: vec![vec![NodeId::from("a"), NodeId::from("b")]],
        };

        let extent = layer_extent(&layout.layers[0], &layout);
        assert_eq!(extent, Some((5.0, 30.0)));
    }
}
