//! Paints the node graph on a central panel with [`egui::Painter`] directly
//! (rects, title text, straight edge lines) rather than a node-graph widget
//! -- see the plan's Chunk D decision: our nodes are nested cluster boxes
//! with cross-cluster edges, which a flat pin-to-pin widget (egui_snarl)
//! fights rather than helps.
//!
//! Pan/zoom is view-only state ([`Transform`]) that lives in the eframe
//! glue, never in [`crate::core::app::App`] -- the core stays geometry-free.

use egui::{Align2, Color32, FontId, Pos2, Rect as EguiRect, Sense, StrokeKind, Ui, Vec2};

use crate::core::app::App;
use crate::graph::layout::{LayoutResult, Pos as LPos, Rect as LRect, TITLE_H};
use crate::graph::model::{GitStatus, NodeId, ProjectGraph};
use crate::ui::theme;

/// Zoom lower bound (10%).
pub const MIN_SCALE: f32 = 0.1;
/// Zoom upper bound (500%).
pub const MAX_SCALE: f32 = 5.0;

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

/// Paint the graph into `ui`'s available space: background, edges, then
/// node rects parent-before-child (containment gives natural z-order),
/// leaf/title labels. Handles pan (drag) and zoom (scroll) on the empty
/// canvas, and auto-pans so the focused node stays visible.
pub fn show(ui: &mut Ui, app: &App, layout: &LayoutResult, transform: &mut Transform) {
    let viewport = ui.max_rect();
    let response = ui.allocate_rect(viewport, Sense::click_and_drag());

    handle_pan_zoom(ui, &response, transform);

    if let Some(focus_rect) = layout.rects.get(&app.focus) {
        let screen_rect = transform.to_screen_rect(*focus_rect);
        transform.pan(clamp_into_view(screen_rect, response.rect));
    }

    let painter = ui.painter_at(response.rect);
    painter.rect_filled(response.rect, 0.0, theme::CANVAS_BG);

    paint_edges(&painter, layout, transform);
    for root in app.graph.sorted_roots() {
        paint_node(&painter, &app.graph, layout, transform, &root, &app.focus);
    }
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

fn paint_edges(painter: &egui::Painter, layout: &LayoutResult, transform: &Transform) {
    for edge in &layout.edges {
        let p0 = transform.to_screen_pos(edge.points[0]);
        let p1 = transform.to_screen_pos(edge.points[1]);
        painter.line_segment([p0, p1], theme::edge_stroke());
    }
}

/// Recursively paint `id`'s rect, then its children (parents painted first
/// gives correct back-to-front z-order since children nest inside).
fn paint_node(
    painter: &egui::Painter,
    graph: &ProjectGraph,
    layout: &LayoutResult,
    transform: &Transform,
    id: &NodeId,
    focus: &NodeId,
) {
    let Some(node) = graph.node(id) else {
        return;
    };
    let Some(rect) = layout.rects.get(id) else {
        return;
    };
    let screen_rect = transform.to_screen_rect(*rect);

    if node.children.is_empty() {
        painter.rect(
            screen_rect,
            2.0,
            theme::leaf_fill(node.status),
            theme::leaf_border_stroke(node.status),
            StrokeKind::Inside,
        );
        painter.text(
            screen_rect.center(),
            Align2::CENTER_CENTER,
            &node.display_name,
            FontId::proportional(12.0 * transform.scale.max(0.3)),
            label_color(node.status),
        );
    } else {
        painter.rect(
            screen_rect,
            2.0,
            theme::container_fill(node.status),
            theme::container_border(node.status),
            StrokeKind::Inside,
        );
        let title_rect = EguiRect::from_min_size(
            screen_rect.min,
            Vec2::new(screen_rect.width(), TITLE_H * transform.scale),
        );
        painter.text(
            title_rect.center(),
            Align2::CENTER_CENTER,
            &node.display_name,
            FontId::proportional(12.0 * transform.scale.max(0.3)),
            label_color(node.status),
        );
        for child in graph.sorted_children(id) {
            paint_node(painter, graph, layout, transform, &child, focus);
        }
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

/// Label text color: near-white, readable against every status fill.
fn label_color(_status: GitStatus) -> Color32 {
    Color32::from_rgb(0xea, 0xea, 0xea)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
