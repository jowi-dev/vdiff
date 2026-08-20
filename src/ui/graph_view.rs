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

use std::collections::{HashMap, HashSet};

use egui::{Align2, Color32, FontId, Pos2, Rect as EguiRect, Sense, StrokeKind, Ui, Vec2};

use crate::core::app::App;
use crate::graph::layout::{self, LayoutResult, Pos as LPos, Rect as LRect};
use crate::graph::model::{GitStatus, NodeId, ProjectGraph};
use crate::graph::test_modules::{
    hide_test_modules, nodes_with_changed_tests, test_strips, TestStrip,
};
use crate::review::comments::Comment;
use crate::review::findings::{self, Finding};
use crate::ui::theme;

/// Zoom lower bound (10%).
pub const MIN_SCALE: f32 = 0.1;
/// Zoom upper bound (500%).
pub const MAX_SCALE: f32 = 5.0;

/// Width of a node's colored left-edge stripe conveying its top-level root
/// (screen-space pixels, independent of zoom -- a thin stripe stays legible
/// at every scale rather than shrinking away when zoomed out).
const STRIPE_W: f32 = 4.0;

/// Height of one legend row (there are two: root swatches, then hints).
const LEGEND_H: f32 = 22.0;

/// Outer margin kept between a screen-anchored corner overlay (the legend,
/// the focused-node status chip) and the window edge.
const CORNER_MARGIN: f32 = 8.0;

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
    /// The initial transform the graph view opens with: identity scale,
    /// panned down by [`theme::GRAPH_TOP_PADDING`] so layer 0 doesn't sit
    /// flush against the window's top edge on first open. Distinct from
    /// [`Default::default`] (a true, zero-offset identity, exercised
    /// directly by the transform-math unit tests below) -- this is what
    /// [`crate::ui::eframe_app::VdiffApp::new`] actually seeds
    /// [`crate::ui::eframe_app::VdiffApp`]'s transform with.
    pub fn initial() -> Self {
        Self {
            scale: 1.0,
            offset: Vec2::new(0.0, theme::GRAPH_TOP_PADDING),
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

/// The three test-module-derived pieces of [`show`]'s per-frame state that
/// only change when [`App::graph`] or [`App::show_tests`] does -- `graph`
/// never changes after startup (see `main::run_gui`'s doc), and
/// `show_tests` only flips on [`crate::core::app::Msg::ToggleTests`], which
/// always returns [`crate::core::app::Cmd::Relayout`]. Recomputing these
/// from scratch on every repaint meant a full clone+prune
/// ([`hide_test_modules`]) plus two more whole-graph scans
/// ([`nodes_with_changed_tests`]/[`test_strips`]) dozens of times a second
/// for data that was almost always unchanged since the last frame. The
/// caller (`crate::ui::eframe_app::VdiffApp`) builds this once via
/// [`GraphViewCache::rebuild`] at construction and again whenever it
/// executes a `Cmd::Relayout`, and hands it to [`show`] by reference.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphViewCache {
    /// How many test modules [`hide_test_modules`] would prune -- the
    /// legend's "N test modules hidden/showing" hint (see
    /// `paint_hint_row`).
    pub hidden_test_count: usize,
    /// [`nodes_with_changed_tests`]'s output over the full graph -- the
    /// tested-checkmark badge drawn on a node whose *hidden* matched test
    /// changed, meaningful regardless of `show_tests`.
    pub changed_test_nodes: HashSet<NodeId>,
    /// [`test_strips`]'s output, only ever non-empty while `show_tests` is
    /// on (see [`App::visible_graph`]'s doc for why a matched test's
    /// info moves from `changed_test_nodes`'s badge to this attached strip
    /// once tests are shown).
    pub strips: HashMap<NodeId, TestStrip>,
}

impl GraphViewCache {
    /// Recompute every field from `app.graph`/`app.show_tests`. Cheap
    /// relative to a repaint budget, but not cheap enough to redo on every
    /// frame -- see the struct doc for when the caller should call this.
    pub fn rebuild(app: &App) -> Self {
        let (_, hidden_test_count) = hide_test_modules(&app.graph);
        let changed_test_nodes = nodes_with_changed_tests(&app.graph);
        let strips = if app.show_tests {
            test_strips(&app.graph)
        } else {
            HashMap::new()
        };
        Self {
            hidden_test_count,
            changed_test_nodes,
            strips,
        }
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
///
/// `cache` is [`GraphViewCache`]'s already-computed test-module data --
/// see its doc for why this doesn't recompute it itself.
pub fn show(
    ui: &mut Ui,
    app: &App,
    layout: &LayoutResult,
    transform: &mut Transform,
    last_focus: &mut Option<NodeId>,
    cache: &GraphViewCache,
) {
    let viewport = ui.max_rect();
    let response = ui.allocate_rect(viewport, Sense::click_and_drag());

    handle_pan_zoom(ui, &response, transform);

    if last_focus.as_ref() != Some(&app.focus) {
        if let Some(focus_rect) = layout.rects.get(&app.focus) {
            let screen_rect = transform.to_screen_rect(*focus_rect);
            let padded_viewport = EguiRect::from_min_max(
                Pos2::new(
                    response.rect.left(),
                    response.rect.top() + theme::GRAPH_TOP_PADDING,
                ),
                response.rect.max,
            );
            transform.pan(clamp_into_view(screen_rect, padded_viewport));
        }
        *last_focus = Some(app.focus.clone());
    }

    let painter = ui.painter_at(response.rect);
    painter.rect_filled(response.rect, 0.0, theme::CANVAS_BG);

    let node_overlay = NodeOverlay {
        tested: &cache.changed_test_nodes,
        strips: &cache.strips,
        show_tests: app.show_tests,
        reviewed: &app.reviewed,
        findings: &app.findings,
        comments: &app.comments,
    };

    paint_band_separators(&painter, layout, transform, response.rect);
    paint_edges(&painter, layout, transform, &app.focus);
    for layer in &layout.layers {
        for id in layer {
            paint_node(
                &painter,
                &app.graph,
                layout,
                transform,
                id,
                &app.focus,
                &node_overlay,
            );
        }
    }

    paint_legend(&painter, app, layout, response.rect, cache.hidden_test_count);
    paint_focus_status(&painter, app, response.rect);
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

/// The graph's total horizontal extent in layout space: the span from the
/// leftmost rect's left edge to the rightmost rect's right edge across
/// every drawn node. `graph::layout` already centers every row relative to
/// every other row within this same span, so it's exactly the width the
/// initial view (see [`initial_x_offset`]) should center in the viewport.
/// `None` for an empty layout (nothing to center).
pub fn graph_width(layout: &LayoutResult) -> Option<f32> {
    layout
        .rects
        .values()
        .fold(None, |acc: Option<(f32, f32)>, rect| {
            let left = rect.origin.x;
            let right = rect.origin.x + rect.size.w;
            Some(match acc {
                None => (left, right),
                Some((l, r)) => (l.min(left), r.max(right)),
            })
        })
        .map(|(left, right)| right - left)
}

/// The `Transform::offset.x` that centers a `graph_width`-wide (layout
/// space) graph at `scale` horizontally within a `viewport_width`-wide
/// viewport -- used once, on the very first frame, so the graph opens
/// centered instead of pinned to the left edge (see
/// [`crate::ui::eframe_app::VdiffApp`]'s `initial_view_centered` field for
/// why this must run exactly once, not every frame: recomputing it after
/// the user has panned would fight their own pan, the same flicker
/// [`show`]'s `last_focus` gating avoids for auto-pan).
///
/// When the graph is wider than the viewport, the naive centered value
/// goes negative enough to crop equally off both sides -- clamped instead
/// to `min_left_margin`, so a graph too wide to fit opens left-aligned
/// with a little breathing room (reading left-to-right from layer 0)
/// rather than starting mid-graph with its first layer already scrolled
/// off to the left.
pub fn initial_x_offset(
    viewport_width: f32,
    graph_width: f32,
    scale: f32,
    min_left_margin: f32,
) -> f32 {
    let centered = (viewport_width - graph_width * scale) / 2.0;
    if centered < 0.0 {
        min_left_margin
    } else {
        centered
    }
}

/// The offset delta (screen-space) needed to bring `focus_rect`
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

/// Per-node paint state [`paint_node`] needs beyond the graph/layout
/// themselves, bundled so the function stays under clippy's argument-count
/// limit: the changed-test hidden-mode badge set, the shown-mode
/// attached-strip map, whether `show_tests` is on at all (which picks
/// between the two -- see [`paint_node`]'s doc), and the set of node ids
/// marked reviewed (issue #4) -- desaturates a node's fill without
/// touching its status stripe or badges.
struct NodeOverlay<'a> {
    tested: &'a HashSet<NodeId>,
    strips: &'a HashMap<NodeId, TestStrip>,
    show_tests: bool,
    reviewed: &'a HashSet<NodeId>,
    /// AI review findings (issue #5), keyed by node -- painted as a small
    /// count+severity badge (see [`findings::badge`]) at the main rect's
    /// top-left corner, opposite the existing tested-badge's top-right
    /// corner so the two never overlap.
    findings: &'a HashMap<NodeId, Vec<Finding>>,
    /// Review comments (issue #14, see [`crate::review::comments::map_comments`]),
    /// keyed by node -- painted as a small violet count badge (see
    /// [`paint_comments_badge`]) at the main rect's bottom-right corner, the
    /// one corner the findings badge (top-left), tested checkmark
    /// (top-right), and attached test strip (below `main_rect`) never
    /// touch.
    comments: &'a HashMap<NodeId, Vec<Comment>>,
}

/// Paint `id`'s rect: status fill/border, a left-edge stripe in its
/// top-level root's hue, a truncated-to-fit abbreviated label, and either a
/// small green "tested" badge (hidden-mode hint) or -- once `show_tests` is
/// on and `strips` has an entry for `id` -- an attached bottom strip
/// showing the matched test module's own short name in its own status
/// color (see [`crate::graph::test_modules::test_strips`]/
/// [`crate::graph::layout::layout_with_test_strips`], which is what makes
/// room for the strip in `layout.rects[id]`'s height in the first place).
/// The combined box stays one focusable node -- the strip is display-only,
/// navigation never lands on it separately. Draws the focus ring around the
/// whole (possibly combined) box on top if `id` is `focus`. If `id` is in
/// [`NodeOverlay::reviewed`], the main rect's fill (only the fill -- status
/// border stroke, root stripe, and badges are untouched) is desaturated via
/// [`theme::dim_reviewed`], the visual cue that this node's been marked
/// reviewed (`v`, see [`crate::core::app::Msg::ToggleReviewed`]). Also
/// paints a small violet comment-count badge at the bottom-right corner if
/// [`NodeOverlay::comments`] has an entry for `id` (issue #14) -- see
/// [`paint_comments_badge`] for why that corner never collides with any of
/// this function's other badges/decorations.
fn paint_node(
    painter: &egui::Painter,
    graph: &ProjectGraph,
    layout: &LayoutResult,
    transform: &Transform,
    id: &NodeId,
    focus: &NodeId,
    overlay: &NodeOverlay,
) {
    let Some(node) = graph.node(id) else {
        return;
    };
    let Some(rect) = layout.rects.get(id) else {
        return;
    };
    let screen_rect = transform.to_screen_rect(*rect);
    let strip = overlay.strips.get(id);

    let main_rect = match strip {
        Some(_) => {
            let strip_h = layout::TEST_STRIP_H * transform.scale;
            EguiRect::from_min_size(
                screen_rect.min,
                Vec2::new(
                    screen_rect.width(),
                    (screen_rect.height() - strip_h).max(0.0),
                ),
            )
        }
        None => screen_rect,
    };

    let fill = if overlay.reviewed.contains(id) {
        theme::dim_reviewed(theme::leaf_fill(node.status))
    } else {
        theme::leaf_fill(node.status)
    };
    painter.rect(
        main_rect,
        2.0,
        fill,
        theme::leaf_border_stroke(node.status),
        StrokeKind::Inside,
    );

    let root_id = graph.top_level_root(id);
    let stripe_color = theme::root_hue_color(&root_id.to_string());
    let stripe_rect =
        EguiRect::from_min_size(main_rect.min, Vec2::new(STRIPE_W, main_rect.height()));
    painter.rect_filled(stripe_rect, 0.0, stripe_color);

    let label = theme::abbreviated_label(&id.to_string(), &root_id.to_string(), &node.display_name);
    let font_size = 12.0 * transform.scale.max(0.3);
    let available_px = main_rect.width() - STRIPE_W - 2.0 * LABEL_PAD;
    let label = fit_label(&label, available_px, font_size);
    painter.text(
        main_rect.center(),
        Align2::CENTER_CENTER,
        &label,
        FontId::proportional(font_size),
        label_color(node.status),
    );

    if let Some(strip) = strip {
        paint_test_strip(painter, screen_rect, main_rect, transform, strip);
    } else if !overlay.show_tests && overlay.tested.contains(id) {
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

    if let Some(findings) = overlay.findings.get(id) {
        paint_findings_badge(painter, main_rect, transform, findings);
    }

    if let Some(comments) = overlay.comments.get(id) {
        paint_comments_badge(painter, main_rect, transform, comments);
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

/// Paint the attached test-module strip below `main_rect`, filling the rest
/// of `screen_rect` (the combined box) with the test's own status color, a
/// separating top border, and its short display name -- see
/// [`paint_node`]'s doc.
fn paint_test_strip(
    painter: &egui::Painter,
    screen_rect: EguiRect,
    main_rect: EguiRect,
    transform: &Transform,
    strip: &TestStrip,
) {
    let strip_rect = EguiRect::from_min_max(
        Pos2::new(screen_rect.left(), main_rect.bottom()),
        screen_rect.max,
    );
    painter.rect_filled(strip_rect, 0.0, theme::leaf_fill(strip.status));
    painter.line_segment(
        [strip_rect.left_top(), strip_rect.right_top()],
        theme::leaf_border_stroke(strip.status),
    );

    let font_size = (10.0 * transform.scale.max(0.3)).max(6.0);
    let label = fit_label(
        &strip.label,
        strip_rect.width() - 2.0 * LABEL_PAD,
        font_size,
    );
    painter.text(
        strip_rect.center(),
        Align2::CENTER_CENTER,
        &label,
        FontId::proportional(font_size),
        label_color(strip.status),
    );
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

/// Paint `findings`'s count+severity badge (issue #5) at `main_rect`'s
/// top-left corner, just past the root-hue stripe -- a small filled circle
/// in [`theme::severity_color`] of the highest severity present (see
/// [`findings::badge`]), with the count as white text on top. Anchored
/// opposite the existing tested-checkmark badge's top-right corner (and
/// always on `main_rect`, never `screen_rect`, so it never collides with an
/// attached test strip below) so the two badges -- plus the reviewed-dimmed
/// fill, which only touches the fill color, never a badge -- all stay
/// legible together. A no-op if `findings` is empty (shouldn't happen --
/// callers only reach this with a non-empty `overlay.findings` entry -- but
/// defended rather than assumed).
fn paint_findings_badge(
    painter: &egui::Painter,
    main_rect: EguiRect,
    transform: &Transform,
    findings: &[Finding],
) {
    let Some((count, severity)) = findings::badge(findings) else {
        return;
    };
    let radius = (7.0 * transform.scale.max(0.3)).max(5.0);
    let center = main_rect.min + Vec2::new(STRIPE_W + radius + 2.0, radius + 2.0);
    painter.circle_filled(center, radius, theme::severity_color(severity));
    painter.text(
        center,
        Align2::CENTER_CENTER,
        format!("{count}"),
        FontId::proportional((radius * 1.1).max(6.0)),
        Color32::WHITE,
    );
}

/// Paint `comments`'s count badge (issue #14) at `main_rect`'s
/// bottom-right corner: a small filled circle in [`theme::COMMENT_BADGE_COLOR`]
/// (the same violet `vdiff.nvim` highlights a commented range with), count
/// as white text on top. Bottom-right is the one corner none of the other
/// three badge/decoration spots ever reach: the findings badge sits
/// top-left (just past the root-hue stripe), the tested checkmark sits
/// top-right, and an attached test strip (when `show_tests` is on) is
/// painted below `main_rect` entirely, never overlapping it -- so all four
/// can coexist legibly on the same node. A no-op if `comments` is empty
/// (shouldn't happen -- callers only reach this with a non-empty
/// `overlay.comments` entry -- but defended rather than assumed).
fn paint_comments_badge(
    painter: &egui::Painter,
    main_rect: EguiRect,
    transform: &Transform,
    comments: &[Comment],
) {
    if comments.is_empty() {
        return;
    }
    let radius = (7.0 * transform.scale.max(0.3)).max(5.0);
    let center = main_rect.max - Vec2::new(radius + 2.0, radius + 2.0);
    painter.circle_filled(center, radius, theme::COMMENT_BADGE_COLOR);
    painter.text(
        center,
        Align2::CENTER_CENTER,
        format!("{}", comments.len()),
        FontId::proportional((radius * 1.1).max(6.0)),
        Color32::WHITE,
    );
}

/// The legend, anchored to the bottom-LEFT corner of the screen (not
/// world/graph space -- it must stay put regardless of pan/zoom, and
/// bottom-left leaves the bottom-right corner free for
/// [`paint_focus_status`]). Two rows over an [`theme::overlay_chip_bg`]
/// backing chip: root-hue swatches, then a hint row -- the hidden/shown
/// test-module count with the `t` key reminder, and two edge-color swatches
/// explaining [`theme::edge_stroke_outgoing`]/[`theme::edge_stroke_incoming`].
fn paint_legend(
    painter: &egui::Painter,
    app: &App,
    layout: &LayoutResult,
    viewport: EguiRect,
    hidden_test_count: usize,
) {
    let chip_height = LEGEND_H * 2.0;
    let chip_rect = EguiRect::from_min_size(
        Pos2::new(
            viewport.left(),
            viewport.bottom() - chip_height - CORNER_MARGIN,
        ),
        Vec2::new(viewport.width(), chip_height),
    );
    painter.rect_filled(chip_rect, 4.0, theme::overlay_chip_bg());

    let root_row_y = chip_rect.top() + LEGEND_H / 2.0;
    let hint_row_y = chip_rect.top() + LEGEND_H + LEGEND_H / 2.0;
    paint_root_legend(painter, &app.graph, layout, viewport, root_row_y);
    paint_hint_row(painter, app, viewport, hint_row_y, hidden_test_count);
}

/// Row 1: every distinct top-level root's name in its
/// [`theme::root_hue_color`], root ids sorted for a stable left-to-right
/// order across frames.
fn paint_root_legend(
    painter: &egui::Painter,
    graph: &ProjectGraph,
    layout: &LayoutResult,
    viewport: EguiRect,
    text_y: f32,
) {
    let mut roots: Vec<NodeId> = layout
        .layers
        .iter()
        .flatten()
        .map(|id| graph.top_level_root(id))
        .collect();
    roots.sort();
    roots.dedup();

    let mut cursor_x = viewport.left() + CORNER_MARGIN;
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

/// Row 2: the `Enter`/`d`/`c`/`v` pane-open/comment/review hint, the review
/// progress readout ("N/M changed modules reviewed" -- see
/// [`App::review_progress`]), the test-module hidden/shown hint (only drawn
/// once there are any test modules to mention at all), then the two
/// edge-color swatches.
fn paint_hint_row(
    painter: &egui::Painter,
    app: &App,
    viewport: EguiRect,
    text_y: f32,
    hidden_count: usize,
) {
    const HINT_COLOR: Color32 = Color32::from_rgb(0xaa, 0xaa, 0xaa);

    let mut cursor_x = viewport.left() + CORNER_MARGIN;

    cursor_x = paint_text(
        painter,
        "Enter: file   d: diff   c: comment   v: review",
        cursor_x,
        text_y,
        HINT_COLOR,
    ) + 20.0;

    let (reviewed_count, total_changed) = app.review_progress();
    if total_changed > 0 {
        let progress = format!("{reviewed_count}/{total_changed} changed modules reviewed");
        cursor_x = paint_text(painter, &progress, cursor_x, text_y, HINT_COLOR) + 20.0;
    }

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

/// The focused-node status readout, anchored to the bottom-RIGHT corner of
/// the screen (screen space, like [`paint_legend`] -- stays put regardless
/// of pan/zoom): the focused node's full, untruncated qualified name plus
/// its first backing file's path, over a [`theme::overlay_chip_bg`] chip.
/// Exists because [`paint_node`]'s own label is abbreviated/truncated to fit
/// the node's box -- this is always the full story, updating as focus
/// moves. A no-op if the focused node somehow isn't in `app.graph` (a
/// synthetic/unknown id shouldn't be focusable, but this is rendering code,
/// so it defends rather than panics).
fn paint_focus_status(painter: &egui::Painter, app: &App, viewport: EguiRect) {
    let Some(node) = app.graph.node(&app.focus) else {
        return;
    };

    let name_color = label_color(node.status);
    const PATH_COLOR: Color32 = Color32::from_rgb(0xaa, 0xaa, 0xaa);
    let path_text = node
        .files
        .first()
        .map(|f| f.path.display().to_string())
        .unwrap_or_default();

    let name_galley = painter.layout_no_wrap(
        app.focus.to_string(),
        FontId::proportional(13.0),
        name_color,
    );
    let path_galley = if path_text.is_empty() {
        None
    } else {
        Some(painter.layout_no_wrap(path_text, FontId::proportional(11.0), PATH_COLOR))
    };

    let content_w = name_galley
        .size()
        .x
        .max(path_galley.as_ref().map_or(0.0, |g| g.size().x));
    let content_h = name_galley.size().y + path_galley.as_ref().map_or(0.0, |g| g.size().y + 2.0);

    let chip_rect = EguiRect::from_min_size(
        Pos2::new(
            viewport.right() - content_w - 2.0 * CORNER_MARGIN,
            viewport.bottom() - content_h - 2.0 * CORNER_MARGIN,
        ),
        Vec2::new(
            content_w + 2.0 * CORNER_MARGIN,
            content_h + 2.0 * CORNER_MARGIN,
        ),
    );
    painter.rect_filled(chip_rect, 4.0, theme::overlay_chip_bg());

    let mut text_y = chip_rect.top() + CORNER_MARGIN;
    let text_x = chip_rect.left() + CORNER_MARGIN;
    let name_h = name_galley.size().y;
    painter.galley(Pos2::new(text_x, text_y), name_galley, name_color);
    text_y += name_h + 2.0;
    if let Some(galley) = path_galley {
        painter.galley(Pos2::new(text_x, text_y), galley, PATH_COLOR);
    }
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
    fn graph_width_none_for_empty_layout() {
        let layout = LayoutResult {
            rects: HashMap::new(),
            edges: Vec::new(),
            layers: Vec::new(),
            rows: Vec::new(),
        };
        assert_eq!(graph_width(&layout), None);
    }

    #[test]
    fn graph_width_spans_leftmost_to_rightmost_rect() {
        let mut rects = HashMap::new();
        rects.insert(
            NodeId::from("a"),
            LRect {
                origin: LPos { x: 10.0, y: 0.0 },
                size: layout::Size { w: 20.0, h: 10.0 },
            },
        );
        rects.insert(
            NodeId::from("b"),
            LRect {
                origin: LPos { x: 100.0, y: 0.0 },
                size: layout::Size { w: 30.0, h: 10.0 },
            },
        );
        let layout = LayoutResult {
            rects,
            edges: Vec::new(),
            layers: Vec::new(),
            rows: Vec::new(),
        };
        // Leftmost edge is a's origin (10.0), rightmost edge is b's origin
        // + width (100.0 + 30.0 = 130.0) -- span is 120.0.
        assert_eq!(graph_width(&layout), Some(120.0));
    }

    #[test]
    fn initial_x_offset_centers_a_narrower_graph() {
        // 800px viewport, 400px graph at scale 1.0 -- 200px margin each side.
        assert_eq!(initial_x_offset(800.0, 400.0, 1.0, 24.0), 200.0);
    }

    #[test]
    fn initial_x_offset_accounts_for_scale() {
        // Same graph zoomed to 2x is 800px wide -- exactly fills the
        // viewport, so it centers at offset 0.
        assert_eq!(initial_x_offset(800.0, 400.0, 2.0, 24.0), 0.0);
    }

    #[test]
    fn initial_x_offset_clamps_to_left_margin_when_graph_wider_than_viewport() {
        // 2000px graph in an 800px viewport centers to a large negative
        // offset (-600) -- clamped up to the left margin instead, so the
        // graph opens left-aligned with breathing room rather than cropped
        // symmetrically off both edges.
        assert_eq!(initial_x_offset(800.0, 2000.0, 1.0, 24.0), 24.0);
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
            rows: vec![vec![NodeId::from("a"), NodeId::from("b")]],
        };

        let extent = layer_extent(&layout.layers[0], &layout);
        assert_eq!(extent, Some((5.0, 30.0)));
    }
}
