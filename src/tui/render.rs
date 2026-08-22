//! Pure(-ish) `ratatui` rendering for the `--tui` graph screen: a git-log-
//! style vertical rail DAG (issue #16 phase 2), replacing the phase-1
//! focused-neighborhood view (three columns of dependents/focused/
//! dependencies) after real use found that view "less helpful than just
//! looking at the directory directly" -- it couldn't show the big picture
//! or zoom in on it. This screen instead shows every visible module as one
//! row, top to bottom in the graph's existing layer order (see
//! [`crate::graph::layers`]), with a left-hand rail gutter drawing the
//! dependency edges between rows the way `git log --graph`/`jj log` draw
//! commit ancestry -- see [`crate::graph::rails`] for the pure column-
//! layout algorithm this paints, and [`crate::core::rail_view`] for the
//! fold-by-namespace "zoom out" mechanic that can collapse a whole
//! namespace's rows into one.
//!
//! Every function here takes `&core::App`/a `&mut Frame` and paints;
//! nothing here mutates `App` or performs IO, so this is exercised entirely
//! through `ratatui::backend::TestBackend` in this module's tests, with no
//! real terminal involved. The one exception to "no mutation" is scroll
//! bookkeeping: [`clamp_scroll`] is a pure function of its inputs, but the
//! *value* it returns is threaded through [`crate::tui::TuiState`] by the
//! caller (`crate::tui::event_loop`), the same way [`file_view_visible_rows`]
//! already feeds `App::viewport_rows` -- see that call site's own comment.

use std::collections::HashMap;

use ratatui::layout::{Alignment, Constraint, Direction as LayoutDirection, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::core::app::{App, Pane, Screen};
use crate::core::diff_state::DiffMode;
use crate::core::file_view::FileViewState;
use crate::core::rail_view::{self, RailRow};
use crate::graph::canvas::{self, CanvasRole, Channel};
use crate::graph::model::{GitStatus, NodeId};
use crate::graph::plane::{self, PlaneLayout};
use crate::graph::plane_edges::{self, PlaneEdges};
use crate::graph::rails::{self, RailRole};
use crate::graph::sugiyama::{self, SlotId};
use crate::review::findings::Severity;
use crate::tui::highlight;
use crate::tui::ViewMode;

/// Warm accent for a rail cell belonging to the focused node's own outgoing
/// (dependency) edges -- the same RGB the GUI's
/// `crate::ui::theme::EDGE_OUTGOING` paints, duplicated here rather than
/// imported since `crate::ui` sits behind the `gui` feature and this module
/// must build under `--no-default-features --features tui` alone.
const RAIL_OUTGOING: Color = Color::Rgb(0xe0, 0x8a, 0x3d);
/// Cool accent for a rail cell belonging to the focused node's own incoming
/// (dependent) edges -- mirrors `crate::ui::theme::EDGE_INCOMING`.
const RAIL_INCOMING: Color = Color::Rgb(0x4d, 0xc8, 0xe8);
/// Dim color for every rail cell not touching the focused node -- keeps a
/// dense gutter readable (see the module doc/task brief's "visual
/// hierarchy" requirement).
const RAIL_DIM: Color = Color::DarkGray;
/// How many rows of buffer [`clamp_scroll`] tries to keep between the
/// focused row and the viewport's top/bottom edge, when the viewport is
/// tall enough to afford it.
const SCROLL_MARGIN: usize = 2;

/// Height in rows of the bottom legend/status strip, constant across every
/// screen so [`file_view_visible_rows`] (which needs to agree with what
/// [`draw`] actually leaves for content) doesn't have to be threaded
/// through separately.
pub const LEGEND_HEIGHT: u16 = 5;

/// Height in rows of the file pane's own header line (path + deleted/
/// changed marker), subtracted from the file pane's block interior before
/// computing how many lines of content actually fit.
const FILE_HEADER_HEIGHT: u16 = 1;

/// How many lines of file content are visible in a file pane of
/// `terminal_rows` total rows -- the file pane fills everything above the
/// legend strip, minus its own header line and the block's top/bottom
/// borders. Shared between [`draw`] (which must actually render that many
/// lines) and the event loop (which feeds this into
/// [`crate::core::app::App::viewport_rows`] for `Ctrl-d`/`Ctrl-u` half-page
/// scrolling) so the two never disagree about the pane's usable height.
pub fn file_view_visible_rows(terminal_rows: u16) -> usize {
    terminal_rows
        .saturating_sub(LEGEND_HEIGHT)
        .saturating_sub(FILE_HEADER_HEIGHT)
        .saturating_sub(2) // block borders
        .max(1) as usize
}

/// Paint one frame for the current `app` state: the graph screen (the rail
/// DAG or file pane, per [`App::pane`]) or the full-screen diff pane, per
/// [`App::screen`], plus the bottom legend strip and any open edge-picker
/// overlay. `notice`, when set, takes over the legend strip's hint line for
/// this one frame -- see `crate::tui::TuiState::notice`'s doc for why the
/// TUI needs this display-only glue state at all (in short: `eprintln!` is
/// invisible/garbled while the alternate screen owns the terminal).
/// `rail_scroll`/`canvas_scroll` are the rail view's and the canvas/plane
/// views' (which share `canvas_scroll` -- see `crate::tui::TuiState::canvas_scroll`'s
/// doc) current vertical scroll offsets, already clamped by the caller via
/// [`clamp_scroll`] -- see [`rail_visible_rows`]'s doc for why that
/// clamping happens in `crate::tui::event_loop` rather than in here.
/// `canvas_scroll_x` is the canvas/plane views' own horizontal counterpart
/// (issue #18's auto-pan -- see [`clamp_scroll_x`]'s doc), unused by the
/// rail view entirely (it has no horizontal scroll at all). `view_mode`
/// picks which of the three graph screens actually paints (issue #17's maintainer
/// override -- see [`crate::tui::ViewMode`]'s doc); the one not currently
/// showing has no rendering cost paid for it at all.
pub fn draw(
    frame: &mut Frame,
    app: &App,
    notice: Option<&str>,
    rail_scroll: usize,
    canvas_scroll: usize,
    canvas_scroll_x: usize,
    view_mode: ViewMode,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(LEGEND_HEIGHT)])
        .split(area);
    let (main_area, legend_area) = (chunks[0], chunks[1]);

    let mut dropped_edges = 0;
    match app.screen {
        Screen::Graph => {
            if app.pane == Pane::File {
                if let Some(file_view) = &app.file_view {
                    draw_file_view(frame, main_area, file_view);
                } else {
                    frame.render_widget(
                        Paragraph::new("Loading file...").alignment(Alignment::Center),
                        main_area,
                    );
                }
            } else {
                match view_mode {
                    ViewMode::Rail => {
                        dropped_edges = draw_rail_graph(frame, main_area, app, rail_scroll);
                    }
                    ViewMode::Canvas => {
                        dropped_edges = draw_canvas_graph(
                            frame,
                            main_area,
                            app,
                            canvas_scroll,
                            canvas_scroll_x,
                        );
                    }
                    ViewMode::Plane => {
                        dropped_edges =
                            draw_plane_graph(frame, main_area, app, canvas_scroll, canvas_scroll_x);
                    }
                }
            }
        }
        Screen::Diff => draw_diff(frame, main_area, app),
    }

    draw_legend(frame, legend_area, app, notice, dropped_edges, view_mode);

    if app.pane == Pane::Graph {
        draw_picker(frame, area, app);
    }
}

/// How many screen lines fit in a terminal of `terminal_rows` total rows --
/// the rail area fills everything above the legend strip, with no border/
/// header of its own (unlike [`file_view_visible_rows`]'s file pane, the
/// rail view has no per-pane chrome eating into it, so this is just
/// [`LEGEND_HEIGHT`] subtracted). Shared between [`draw`] (which must
/// actually render that many lines) and `crate::tui::event_loop` (which
/// feeds this into [`clamp_scroll`] every frame, mirroring exactly how
/// `event_loop` already threads [`file_view_visible_rows`] into
/// `App::viewport_rows` before each `terminal.draw` call). Named
/// `rail_visible_rows` for historical continuity with that pattern, but
/// note the unit is *screen lines*, not module rows -- see
/// [`DisplayLine`]'s doc for why those two counts can differ.
pub fn rail_visible_rows(terminal_rows: u16) -> usize {
    terminal_rows.saturating_sub(LEGEND_HEIGHT).max(1) as usize
}

/// Adjust `scroll` (the previous frame's topmost visible line index) by as
/// little as possible so `focus_idx` stays within [`SCROLL_MARGIN`] lines of
/// the viewport's top/bottom edge -- a scroll-margin policy (like `vim`'s
/// `scrolloff`), not a center-on-jump one: a `gd`/`gr` jump to a far row
/// still lands inside the margin rather than dead-center, but since this
/// runs fresh every frame from the current `scroll`/`focus_idx` regardless
/// of *why* focus moved, a far jump is still guaranteed visible -- no
/// special-casing needed for `gd`/`gr` versus plain `j`/`k`. Degrades
/// gracefully when `viewport_height` is too short to afford a full margin
/// on both ends (halves the margin rather than refusing to scroll at all).
/// Always returns a value in `0..=total_rows.saturating_sub(viewport_height)`.
///
/// Generic over what `focus_idx`/`total_rows` count: `crate::tui::event_loop`
/// calls this with *display-line* indices (see [`DisplayLine`]/
/// [`focus_display_line`]/[`display_line_count`]), not raw module-row
/// indices, so that a band separator's extra screen line is accounted for
/// in the same units [`draw_rail_graph`] actually renders in -- see that
/// function's doc for the bug this fixes (scroll math that only counted
/// rows undercounted how tall the rendered content actually was, letting
/// the focused row scroll off past the bottom edge).
pub fn clamp_scroll(
    scroll: usize,
    focus_idx: usize,
    total_rows: usize,
    viewport_height: usize,
) -> usize {
    if viewport_height == 0 {
        return 0;
    }
    let max_scroll = total_rows.saturating_sub(viewport_height);
    let margin = SCROLL_MARGIN.min(viewport_height.saturating_sub(1) / 2);
    let scroll = scroll.min(max_scroll);

    let min_visible = scroll + margin;
    let max_visible = scroll + viewport_height - 1 - margin;

    let adjusted = if focus_idx < min_visible {
        focus_idx.saturating_sub(margin)
    } else if focus_idx > max_visible {
        focus_idx + margin + 1 - viewport_height
    } else {
        scroll
    };
    adjusted.min(max_scroll)
}

/// How many columns of margin [`clamp_scroll_x`] tries to keep between the
/// focused node's own `[x, x+width)` range and the viewport's left/right
/// edge -- the horizontal analog of [`SCROLL_MARGIN`], for the canvas
/// screen's issue #18 auto-pan (see that fix's own doc note in
/// `crate::graph::sugiyama` on why the canvas needed 2D scrolling at all:
/// `sugiyama::layout` lays bands out in unbounded char space, so any band
/// wider than the terminal used to just clip at the right edge with no way
/// to reach what fell off it).
const COLUMN_MARGIN: usize = 4;

/// The horizontal counterpart to [`clamp_scroll`]: adjust `scroll_x` (the
/// previous frame's leftmost visible column) by as little as possible so
/// the focused node's entire `[focus_x, focus_x + focus_width)` range stays
/// within [`COLUMN_MARGIN`] columns of the viewport's left/right edge.
/// Unlike [`clamp_scroll`], there is no fixed `total_columns` to clamp
/// against from above -- `crate::graph::sugiyama`'s layout is unbounded
/// char space, so scrolling arbitrarily far right to reach a node that's
/// genuinely out there is exactly the point -- but `scroll_x` can never go
/// negative, so `0` is always the effective floor (enforced by the
/// `usize` arithmetic itself, via `saturating_sub`).
pub fn clamp_scroll_x(
    scroll_x: usize,
    focus_x: usize,
    focus_width: usize,
    viewport_width: usize,
) -> usize {
    if viewport_width == 0 {
        return 0;
    }
    let margin = COLUMN_MARGIN.min(viewport_width.saturating_sub(1) / 2);
    let focus_end = focus_x + focus_width;

    let min_visible = scroll_x + margin;
    let max_visible = scroll_x + viewport_width.saturating_sub(margin);

    if focus_x < min_visible {
        focus_x.saturating_sub(margin)
    } else if focus_end > max_visible {
        focus_end + margin - viewport_width
    } else {
        scroll_x
    }
}

/// Slice `line`'s spans down to the half-open column window
/// `[start, start + width)`, in character units (not bytes -- see the
/// `chars()`/`char_indices` use throughout), preserving each retained
/// span's own style. The fiddly part [`crate::tui::mod`]'s auto-pan (issue
/// #18) needed: a band or channel line built from several differently-
/// styled spans (badges, status colors, bolded focus) must still cut
/// cleanly mid-span at either edge of the window without losing style
/// information or panicking on multi-byte characters -- see the module's
/// own tests for the shapes this handles (a cut landing inside a single
/// span, a window spanning several spans, and non-ASCII content).
pub fn slice_line_columns(line: &Line<'static>, start: usize, width: usize) -> Line<'static> {
    let end = start.saturating_add(width);
    let mut spans = Vec::new();
    let mut col = 0usize;
    for span in &line.spans {
        let char_count = span.content.chars().count();
        let span_start = col;
        let span_end = col + char_count;
        col = span_end;
        if span_end <= start || span_start >= end {
            continue;
        }
        let take_start = start.saturating_sub(span_start).min(char_count);
        let take_end = end.saturating_sub(span_start).min(char_count);
        if take_start >= take_end {
            continue;
        }
        let sliced: String = span
            .content
            .chars()
            .skip(take_start)
            .take(take_end - take_start)
            .collect();
        spans.push(Span::styled(sliced, span.style));
    }
    Line::from(spans)
}

/// One screen line the rail view's viewport scrolls over: either a band
/// separator (see [`band_separator_line`]) or an actual module/namespace
/// row (an index into whatever `Vec<(RailRow, usize)>` [`build_display_lines`]
/// was built from). A separator consumes a screen line of its own, just
/// like a row does -- [`draw_rail_graph`] used to slice its *row* list by
/// `rail_scroll..rail_scroll + area.height` and insert a separator line
/// wherever the layer changed within that window, which could push the
/// total rendered line count past `area.height` whenever more than one
/// layer transition fell inside the visible window; `ratatui::Paragraph`
/// then silently clipped the overflow off the bottom, which could push the
/// focused row itself off-screen despite `clamp_scroll`'s margin -- because
/// that margin was computed in row-index space, one unit per row, with no
/// idea separators existed at all. Building this list once up front and
/// scrolling/clamping in *its* index space (one unit per screen line,
/// separators included) fixes both sides of that mismatch at the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayLine {
    /// A layer-band separator, carrying that layer's index.
    Separator(usize),
    /// A module/namespace row, carrying its index into the `rows` slice
    /// [`build_display_lines`] was built from.
    Row(usize),
}

/// Build the full (unscrolled) display-line list for `rows` (as returned by
/// [`rail_view::visible_rows_with_layers`]): a [`DisplayLine::Separator`] is
/// inserted immediately before the first row of every layer -- including
/// the very first layer in the list, so the top of a *never-scrolled* rail
/// view still opens with a band label -- and nowhere else. Building this
/// once, up front, over the *entire* row list (rather than re-deriving it
/// per visible window, the way the old per-frame `prev_layer` peek in
/// `draw_rail_graph` did) means a scrolled-to slice of it never needs to
/// guess whether a separator belongs at its top: the separator is already
/// at its one fixed position in this canonical list, so slicing either
/// includes it (if the slice starts at or before it) or doesn't (if the
/// slice starts after it) -- never duplicated, never missing.
fn build_display_lines(rows: &[(RailRow, usize)]) -> Vec<DisplayLine> {
    let mut lines = Vec::with_capacity(rows.len() + rows.len().min(1));
    let mut prev_layer: Option<usize> = None;
    for (idx, (_, layer)) in rows.iter().enumerate() {
        if prev_layer != Some(*layer) {
            lines.push(DisplayLine::Separator(*layer));
        }
        prev_layer = Some(*layer);
        lines.push(DisplayLine::Row(idx));
    }
    lines
}

/// The index of `focus`'s row within `rows`' display-line list (see
/// [`build_display_lines`]), or `None` if `focus` isn't present in `rows`
/// at all. What `crate::tui::event_loop` feeds into [`clamp_scroll`] as
/// `focus_idx` every frame, so the scroll margin is computed in the same
/// display-line space [`draw_rail_graph`] actually renders in -- see
/// [`DisplayLine`]'s doc for why row-index space alone isn't enough.
pub fn focus_display_line(rows: &[(RailRow, usize)], focus: &NodeId) -> Option<usize> {
    build_display_lines(rows)
        .iter()
        .position(|line| matches!(line, DisplayLine::Row(idx) if rows[*idx].0.id() == focus))
}

/// The total number of display lines (rows *and* separators) `rows` renders
/// as -- what `crate::tui::event_loop` feeds into [`clamp_scroll`] as
/// `total_rows` instead of `rows.len()`, which would undercount by exactly
/// the separator count. See [`DisplayLine`]'s doc.
pub fn display_line_count(rows: &[(RailRow, usize)]) -> usize {
    build_display_lines(rows).len()
}

/// One node's rendered line: a status-colored bullet, `label`, and
/// trailing badges -- changed-test checkmark, findings count/severity,
/// comment count, reviewed mark. `label` is `id`'s
/// [`rail_view::disambiguated_labels`] entry rather than
/// `node.display_name` directly, so two distinct ids that happen to share
/// a bare display name (e.g. two different `docs` directories) render
/// distinguishably instead of as byte-identical text -- see that
/// function's doc for when/how it qualifies a name. Shared by every row
/// [`row_line`] builds for a [`RailRow::Node`], so the badge set stays
/// exactly what the GUI's own `crate::ui::graph_view::paint_node`/badge
/// functions paint. A reviewed node's entire line gets [`Modifier::DIM`]
/// on top of its normal colors -- the terminal analogue of the GUI's
/// `crate::ui::theme::dim_reviewed` (which blends a box's fill 1/3 toward
/// gray): a true partial color blend isn't expressible per-glyph in a
/// 16/256-color terminal palette, so `DIM` is the closest "still legible,
/// visibly muted" terminal equivalent.
fn node_line(app: &App, id: &NodeId, label: &str) -> Line<'static> {
    let Some(node) = app.graph.node(id) else {
        return Line::from(id.to_string());
    };
    let mut spans = vec![
        Span::styled("● ", Style::default().fg(status_color(node.status))),
        Span::raw(label.to_string()),
    ];
    if crate::graph::test_modules::matched_test_module(&app.graph, id)
        .is_some_and(|test_id| app.graph.node(&test_id).is_some())
    {
        spans.push(Span::styled(" ✓t", Style::default().fg(Color::Green)));
    }
    if let Some((count, severity)) = crate::review::findings::badge(app.findings_for(id)) {
        spans.push(Span::styled(
            format!(" ⚑{count}"),
            Style::default().fg(severity_color(severity)),
        ));
    }
    if let Some(comments) = app.comments.get(id) {
        if !comments.is_empty() {
            spans.push(Span::styled(
                format!(" 💬{}", comments.len()),
                Style::default().fg(Color::Magenta),
            ));
        }
    }
    if app.reviewed.contains(id) {
        spans.push(Span::styled(" ✔", Style::default().fg(Color::Cyan)));
    }
    if app.reviewed.contains(id) {
        for span in &mut spans {
            span.style = span.style.add_modifier(Modifier::DIM);
        }
    }
    Line::from(spans)
}

fn status_color(status: GitStatus) -> Color {
    match status {
        GitStatus::Unchanged => Color::Gray,
        GitStatus::Added => Color::Green,
        GitStatus::Modified => Color::Yellow,
        GitStatus::Deleted => Color::Red,
    }
}

fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Low => Color::Blue,
        Severity::Medium => Color::Yellow,
        Severity::High => Color::Red,
    }
}

/// The git-log-style rail DAG: one row per visible module (see
/// [`crate::core::rail_view::visible_rows`]), in the graph's existing layer
/// order, with a left-hand rail gutter (see [`crate::graph::rails`]) drawing
/// the dependency edges between rows -- big picture by default (issue #16
/// phase 2's whole point), zoomed in via fold-by-namespace (`h`/`l`) and
/// scrolled via `j`/`k`/`gd`/`gr`. Slices [`build_display_lines`]'s
/// canonical (row + separator) line list by
/// `rail_scroll..rail_scroll + area.height`, so exactly as many screen
/// lines as fit are ever built into [`Line`]s regardless of how many band
/// separators happen to fall inside that window -- see [`DisplayLine`]'s
/// doc for the bug this fixes (the previous version sliced the *row* list
/// and inserted separators as it went, which could push the actual
/// rendered line count past `area.height`). The caller
/// (`crate::tui::event_loop`) has already clamped `rail_scroll` in this
/// same display-line space (see [`clamp_scroll`]/[`focus_display_line`])
/// so the focused row is guaranteed inside the window. Returns the rail
/// layout's `dropped_edges` count (`0` unless the gutter's width cap kicked
/// in -- see [`crate::graph::rails::compute`]'s doc) so [`draw`] can pass it
/// on to [`draw_legend`]'s `+N edges` hint.
fn draw_rail_graph(frame: &mut Frame, area: Rect, app: &App, rail_scroll: usize) -> usize {
    let rows = rail_view::visible_rows_with_layers(&app.graph, &app.layers, &app.fold_collapsed);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("(no visible nodes)").alignment(Alignment::Center),
            area,
        );
        return 0;
    }

    let row_ids: Vec<NodeId> = rows.iter().map(|(row, _)| row.id().clone()).collect();
    let edges = rail_view::collapse_edges(&app.graph, &app.graph.edges, &app.fold_collapsed);
    let rail_layout = rails::compute(&row_ids, &edges, &app.focus, area.width as usize);
    let raw_rows: Vec<RailRow> = rows.iter().map(|(row, _)| row.clone()).collect();
    let labels = rail_view::disambiguated_labels(&app.graph, &raw_rows);

    let display_lines = build_display_lines(&rows);
    let start = rail_scroll.min(display_lines.len().saturating_sub(1));
    let end = (start + area.height as usize).min(display_lines.len());

    let mut lines: Vec<Line> = Vec::with_capacity(end - start);
    for display_line in &display_lines[start..end] {
        match display_line {
            DisplayLine::Separator(layer) => lines.push(band_separator_line(*layer, area.width)),
            DisplayLine::Row(idx) => {
                let (row, _layer) = &rows[*idx];
                let focused = row.id() == &app.focus;
                lines.push(row_line(app, row, &rail_layout, *idx, focused, &labels));
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
    rail_layout.dropped_edges
}

/// A dim `── layer N ──` rule spanning `width` columns, matching the GUI's
/// band-separator convention (a faint horizontal line between layers -- see
/// `crate::ui::graph_view::paint_band_separators`) in spirit, as plain text
/// since there's no line-painting primitive worth reaching for here.
fn band_separator_line(layer: usize, width: u16) -> Line<'static> {
    let label = format!(" layer {} ", layer + 1);
    let dashes = (width as usize).saturating_sub(label.len()) / 2;
    let text = format!(
        "{}{}{}",
        "─".repeat(dashes),
        label,
        "─".repeat((width as usize).saturating_sub(dashes + label.len()))
    );
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

/// One rendered row: the rail gutter (see [`gutter_spans`]), a focus marker,
/// then the row's own content -- [`node_line`] for a [`RailRow::Node`], or
/// a `"name/ (N modules, M changed)"` summary for a [`RailRow::Collapsed`]
/// namespace (see the task brief's fold-by-namespace summary format).
/// `labels` is [`rail_view::disambiguated_labels`]'s output for the current
/// frame's visible row set -- see [`node_line`]'s doc for why the row's
/// bare `display_name` isn't rendered directly.
fn row_line(
    app: &App,
    row: &RailRow,
    rail_layout: &rails::RailLayout,
    row_idx: usize,
    focused: bool,
    labels: &HashMap<NodeId, String>,
) -> Line<'static> {
    let mut spans = gutter_spans(rail_layout, row_idx);
    spans.push(Span::raw(if focused { "▸ " } else { "  " }));
    let label = labels.get(row.id()).map(String::as_str).unwrap_or("");
    match row {
        RailRow::Node(id) => spans.extend(node_line(app, id, label).spans),
        RailRow::Collapsed {
            module_count,
            changed_count,
            ..
        } => spans.extend(collapsed_row_spans(label, *module_count, *changed_count)),
    }
    if focused {
        for span in &mut spans {
            span.style = span.style.add_modifier(Modifier::BOLD);
        }
    }
    Line::from(spans)
}

/// The gutter portion of one row: one styled space/glyph per rail column
/// (`0..rail_layout.columns`), colored per [`RailRole`] -- dim for
/// [`RailRole::Normal`], the two accent colors for the focused node's own
/// edges (see the module doc's `RAIL_*` constants).
fn gutter_spans(rail_layout: &rails::RailLayout, row_idx: usize) -> Vec<Span<'static>> {
    let cells = rail_layout
        .rows
        .get(row_idx)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    (0..rail_layout.columns)
        .map(|column| match cells.iter().find(|c| c.column == column) {
            Some(cell) => Span::styled(
                cell.glyph.to_string(),
                Style::default().fg(role_color(cell.role)),
            ),
            None => Span::raw(" "),
        })
        .collect()
}

fn role_color(role: RailRole) -> Color {
    match role {
        RailRole::Normal => RAIL_DIM,
        RailRole::FocusedOutgoing => RAIL_OUTGOING,
        RailRole::FocusedIncoming => RAIL_INCOMING,
    }
}

/// `"<name>/ (N modules, M changed)"` for a collapsed namespace row -- the
/// task brief's fold summary format. `name` is the namespace's
/// [`rail_view::disambiguated_labels`] entry (see [`node_line`]'s doc on
/// why a plain `display_name` isn't safe to render directly here either).
fn collapsed_row_spans(
    name: &str,
    module_count: usize,
    changed_count: usize,
) -> Vec<Span<'static>> {
    let modules_word = if module_count == 1 {
        "module"
    } else {
        "modules"
    };
    let text = format!("{name}/ ({module_count} {modules_word}, {changed_count} changed)");
    vec![Span::styled(
        text,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]
}

// -- The `--tui` canvas graph screen (issue #17) ---------------------------
//
// A semantic-zoom Sugiyama layout of the same fold-aware visible row set the
// rail view draws (`crate::core::rail_view`), laid out by
// `crate::graph::sugiyama` and routed by `crate::graph::canvas`: bands of
// node labels stacked top to bottom, with a routed inter-band channel
// between each pair. Unlike the rail view's single left-hand gutter, the
// DAG's actual 2D shape is visible here -- multiple parents/children spread
// out left-to-right within a band, edges bending through the channel
// between bands rather than all sharing one vertical rail.
//
// Band-wrap (an overflowing band split into multiple node rows -- see the
// issue's own "hard problems" list) is still NOT implemented: `sugiyama::
// layout` assigns each band's x-coordinates in an unbounded char space with
// no idea what the terminal width even is (see that module's own doc), and
// `draw_canvas_graph` below renders each band as exactly one `Line` with no
// `.wrap()` on its `Paragraph`. What issue #18 replaced is what used to
// happen next: a band wider than the terminal no longer clips at the right
// edge with the rest permanently invisible -- `canvas_scroll_x`
// (`crate::tui::TuiState`) is a real horizontal pan offset, auto-following
// the focused node via `clamp_scroll_x` exactly the way `canvas_scroll`
// already auto-follows it vertically, and every rendered line is sliced to
// the current `[canvas_scroll_x, canvas_scroll_x + width)` window via
// `slice_line_columns` before painting. `h`/`l` (already spatial focus
// movement in this mode) pan the viewport as a side effect of moving focus
// past the visible edge -- there's no separate pan key. A real wrap policy
// (splitting one overflowing band into several node rows) is still future
// work; panning only solves "reach it," not "see the whole band at once."

/// Everything the canvas screen needs, built fresh each frame from `App`
/// state (mirroring [`rail_view::visible_rows_with_layers`]'s own "recompute,
/// don't cache" precedent -- see that module's doc): the pure Sugiyama
/// layout, its routed inter-band channels, the original (fold-collapsed)
/// edge list (needed to recover a dummy slot's true edge for role/color --
/// see [`SlotId::Dummy`]'s doc), a lookup from real node id back to its
/// [`RailRow`] (so label rendering can reuse [`node_line`]/
/// [`collapsed_row_spans`] exactly as the rail view does, keeping the two
/// views' badge/color conventions identical -- see the issue's own note to
/// mirror `crate::ui::graph_view`'s badge/accent semantics), and this
/// frame's [`rail_view::disambiguated_labels`] map (so a name collision
/// between two visible rows -- see that function's doc -- reads
/// disambiguated on the canvas exactly as it does on the rail view).
pub struct CanvasView {
    layout: sugiyama::Layout,
    channels: Vec<Channel>,
    edges: Vec<(NodeId, NodeId)>,
    row_of: HashMap<NodeId, RailRow>,
    labels: HashMap<NodeId, String>,
}

/// Build [`CanvasView`] from `app`'s current fold state -- one band per
/// distinct layer transition in [`rail_view::visible_rows_with_layers`]'s
/// output (matching the rail view's own band-separator grouping exactly),
/// [`rail_view::collapse_edges`] for the edge list, and
/// [`plain_row_text`] as the label function feeding
/// [`sugiyama::layout`]'s width calculation.
pub fn build_canvas_view(app: &App) -> CanvasView {
    let rows = rail_view::visible_rows_with_layers(&app.graph, &app.layers, &app.fold_collapsed);
    let mut bands: Vec<Vec<NodeId>> = Vec::new();
    let mut row_of: HashMap<NodeId, RailRow> = HashMap::new();
    let mut prev_layer: Option<usize> = None;
    for (row, layer) in &rows {
        if prev_layer != Some(*layer) {
            bands.push(Vec::new());
            prev_layer = Some(*layer);
        }
        let id = row.id().clone();
        row_of.insert(id.clone(), row.clone());
        bands.last_mut().expect("just pushed").push(id);
    }
    let raw_rows: Vec<RailRow> = rows.iter().map(|(row, _)| row.clone()).collect();
    let labels = rail_view::disambiguated_labels(&app.graph, &raw_rows);
    let edges = rail_view::collapse_edges(&app.graph, &app.graph.edges, &app.fold_collapsed);
    let layout = sugiyama::layout(&bands, &edges, |id| {
        row_of
            .get(id)
            .map(|row| plain_row_text(app, row, &labels))
            .unwrap_or_else(|| id.to_string())
    });
    let channels = canvas::route_channels(&layout, &app.focus);
    CanvasView {
        layout,
        channels,
        edges,
        row_of,
        labels,
    }
}

/// The plain-text content [`node_line`]/[`collapsed_row_spans`] would
/// render for `row` -- what [`build_canvas_view`] feeds
/// [`sugiyama::layout`] as each real node's label width. Reuses those two
/// span-building functions for their text alone (styling is applied
/// separately at actual draw time -- see [`canvas_label_line`]) so the
/// canvas's width estimate always matches the badges the rail view already
/// shows, rather than drifting out of sync with a second, hand-maintained
/// format string. `labels` is [`rail_view::disambiguated_labels`]'s output
/// -- same map [`build_canvas_view`] stores for [`canvas_label_line`]'s use
/// at actual draw time, so the width estimate and the drawn text always
/// agree on which name each row renders.
fn plain_row_text(app: &App, row: &RailRow, labels: &HashMap<NodeId, String>) -> String {
    let label = labels.get(row.id()).map(String::as_str).unwrap_or("");
    let spans = match row {
        RailRow::Node(id) => node_line(app, id, label).spans,
        RailRow::Collapsed {
            module_count,
            changed_count,
            ..
        } => collapsed_row_spans(label, *module_count, *changed_count),
    };
    spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The `(layers, rows)` pair [`crate::core::focus::move_focus`] needs for
/// canvas-mode spatial `h`/`j`/`k`/`l`: one `layers` entry per band (real
/// node ids only, dummies filtered out -- `h`/`l` have no business landing
/// on a bare routing point) and one `rows` entry per band pairing each real
/// node with [`sugiyama::Slot::x_center`] -- the char-space stand-in for the
/// GUI's pixel x-centers, letting `move_focus` run entirely unmodified over
/// this layout the same way it already does over
/// `crate::graph::layout::rows_with_x_centers`'s pixel ones.
pub type CanvasFocusRows = Vec<Vec<(NodeId, f32)>>;

pub fn canvas_focus_grid(app: &App) -> (Vec<Vec<NodeId>>, CanvasFocusRows) {
    let view = build_canvas_view(app);
    let layers: Vec<Vec<NodeId>> = view
        .layout
        .bands
        .iter()
        .map(|band| {
            band.iter()
                .filter_map(|s| s.id.real_id().cloned())
                .collect()
        })
        .collect();
    let rows: Vec<Vec<(NodeId, f32)>> = view
        .layout
        .bands
        .iter()
        .map(|band| {
            band.iter()
                .filter_map(|s| s.id.real_id().map(|id| (id.clone(), s.x_center())))
                .collect()
        })
        .collect();
    (layers, rows)
}

/// One scrollable line of the canvas screen: a band's own label row, or one
/// row of the routed channel immediately below it. Built once per frame
/// over the *entire* view (not just what's visible), the same
/// [`DisplayLine`] precedent [`build_display_lines`] set for the rail view,
/// so scrolling/clamping happens in the same line-index space this actually
/// renders in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanvasLine {
    Label(usize),
    Channel(usize, usize),
}

fn build_canvas_lines(view: &CanvasView) -> Vec<CanvasLine> {
    let mut lines = Vec::new();
    for (band_idx, _band) in view.layout.bands.iter().enumerate() {
        lines.push(CanvasLine::Label(band_idx));
        if let Some(channel) = view.channels.get(band_idx) {
            for row in 0..channel.height {
                lines.push(CanvasLine::Channel(band_idx, row));
            }
        }
    }
    lines
}

/// The display-line index of `focus`'s band, or `None` if `focus` isn't
/// present in the current canvas view at all -- what
/// `crate::tui::event_loop` feeds into [`clamp_scroll`] as `focus_idx`,
/// mirroring [`focus_display_line`]'s role for the rail view.
pub fn focus_canvas_line(view: &CanvasView, focus: &NodeId) -> Option<usize> {
    let band_idx = view
        .layout
        .bands
        .iter()
        .position(|band| band.iter().any(|s| s.id.real_id() == Some(focus)))?;
    build_canvas_lines(view)
        .iter()
        .position(|line| matches!(line, CanvasLine::Label(idx) if *idx == band_idx))
}

/// The total number of display lines the canvas view renders as -- what
/// `crate::tui::event_loop` feeds into [`clamp_scroll`] as `total_rows`,
/// mirroring [`display_line_count`]'s role for the rail view.
pub fn canvas_line_count(view: &CanvasView) -> usize {
    build_canvas_lines(view).len()
}

/// `focus`'s own `(x, width)` in `view`'s char-space layout, or `None` if
/// `focus` isn't present as a real slot in any band -- what
/// `crate::tui::event_loop` feeds into [`clamp_scroll_x`] every frame so
/// the horizontal auto-pan (issue #18) follows focus exactly the way
/// [`focus_canvas_line`] already does for the vertical scroll.
pub fn focused_slot_range(view: &CanvasView, focus: &NodeId) -> Option<(usize, usize)> {
    view.layout
        .bands
        .iter()
        .flat_map(|band| band.iter())
        .find(|slot| slot.id.real_id() == Some(focus))
        .map(|slot| (slot.x, slot.width))
}

fn canvas_role_color(role: CanvasRole) -> Color {
    match role {
        CanvasRole::Normal => RAIL_DIM,
        CanvasRole::FocusedOutgoing => RAIL_OUTGOING,
        CanvasRole::FocusedIncoming => RAIL_INCOMING,
    }
}

/// The edge (true `from`/`to`) a [`SlotId::Dummy`] passes through, recovered
/// from `view.edges` by the dummy's own embedded edge index -- `edge_idx`
/// indexes the exact slice [`sugiyama::layout`] was called with (see
/// [`build_canvas_view`]), so this is always in bounds for a dummy this
/// view actually produced.
fn dummy_role(view: &CanvasView, edge_idx: usize, focus: &NodeId) -> CanvasRole {
    match view.edges.get(edge_idx) {
        Some((from, to)) if from == focus => CanvasRole::FocusedOutgoing,
        Some((from, to)) if to == focus => CanvasRole::FocusedIncoming,
        _ => CanvasRole::Normal,
    }
}

/// One band's label row: every slot's content placed at its assigned
/// `x` column (padded with spaces up to that column), a real node's content
/// coming from [`node_line`]/[`collapsed_row_spans`] (bolded if it's the
/// focused node), a dummy slot rendering as a bare `│` passthrough (in the
/// role of whichever edge it belongs to -- see [`dummy_role`]) so a
/// long edge reads as one continuous rail through the bands it merely
/// passes over, not a gap.
fn canvas_label_line(app: &App, view: &CanvasView, band_idx: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut col = 0usize;
    for slot in &view.layout.bands[band_idx] {
        let pad = slot.x.saturating_sub(col);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        col = col.max(slot.x);
        match &slot.id {
            SlotId::Dummy(edge_idx, _) => {
                let role = dummy_role(view, *edge_idx, &app.focus);
                spans.push(Span::styled(
                    "│".to_string(),
                    Style::default().fg(canvas_role_color(role)),
                ));
                col += 1;
            }
            SlotId::Real(id) => {
                let label = view.labels.get(id).map(String::as_str).unwrap_or("");
                let mut node_spans = match view.row_of.get(id) {
                    Some(RailRow::Node(nid)) => node_line(app, nid, label).spans,
                    Some(RailRow::Collapsed {
                        module_count,
                        changed_count,
                        ..
                    }) => collapsed_row_spans(label, *module_count, *changed_count),
                    None => vec![Span::raw(id.to_string())],
                };
                if id == &app.focus {
                    for span in &mut node_spans {
                        span.style = span.style.add_modifier(Modifier::BOLD);
                    }
                }
                col += slot.width;
                spans.extend(node_spans);
            }
        }
    }
    Line::from(spans)
}

/// One row of a routed [`Channel`]: each occupied column's glyph, colored
/// by [`CanvasRole`], space-padded between them.
fn canvas_channel_line(channel: &Channel, row_idx: usize) -> Line<'static> {
    let cells = channel.rows.get(row_idx).map(Vec::as_slice).unwrap_or(&[]);
    let mut sorted = cells.to_vec();
    sorted.sort_by_key(|c| c.column);
    let mut spans = Vec::new();
    let mut col = 0usize;
    for cell in &sorted {
        let pad = cell.column.saturating_sub(col);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        spans.push(Span::styled(
            cell.glyph.to_string(),
            Style::default().fg(canvas_role_color(cell.role)),
        ));
        col = cell.column + 1;
    }
    Line::from(spans)
}

/// The semantic-zoom Sugiyama canvas: builds a fresh [`CanvasView`] from
/// `app`, then renders exactly the visible window of
/// [`build_canvas_lines`]'s (label + channel) line list, the same
/// scroll-then-slice discipline [`draw_rail_graph`] uses for the rail view
/// (see [`DisplayLine`]'s doc for why lines, not raw bands, are the unit
/// scrolling happens in).
/// Returns the total [`Channel::dropped`] count summed across every routed
/// channel in this frame's view -- [`draw`]'s counterpart to
/// [`draw_rail_graph`]'s own return, feeding the same `dropped_edges`
/// parameter into [`draw_legend`] (see that call site) so the canvas's
/// channel-budget degrade (issue #18) gets the same "+N edges" legend
/// treatment the rail gutter's width cap already has. `canvas_scroll_x` is
/// the horizontal pan offset (issue #18) -- every rendered line is sliced
/// down to `[canvas_scroll_x, canvas_scroll_x + area.width)` via
/// [`slice_line_columns`] before being handed to the `Paragraph`, replacing
/// the old right-edge clip (a band/channel wider than the terminal used to
/// just cut off past the visible width with no way to reach what fell off
/// it) with actual horizontal scrolling.
fn draw_canvas_graph(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    canvas_scroll: usize,
    canvas_scroll_x: usize,
) -> usize {
    let view = build_canvas_view(app);
    let lines_index = build_canvas_lines(&view);
    let dropped_edges: usize = view.channels.iter().map(|c| c.dropped).sum();
    if lines_index.is_empty() {
        frame.render_widget(
            Paragraph::new("(no visible nodes)").alignment(Alignment::Center),
            area,
        );
        return dropped_edges;
    }

    let start = canvas_scroll.min(lines_index.len().saturating_sub(1));
    let end = (start + area.height as usize).min(lines_index.len());

    let mut lines: Vec<Line> = Vec::with_capacity(end - start);
    for line in &lines_index[start..end] {
        let full_line = match line {
            CanvasLine::Label(band_idx) => canvas_label_line(app, &view, *band_idx),
            CanvasLine::Channel(band_idx, row_idx) => {
                let channel = &view.channels[*band_idx];
                canvas_channel_line(channel, *row_idx)
            }
        };
        lines.push(slice_line_columns(
            &full_line,
            canvas_scroll_x,
            area.width as usize,
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
    dropped_edges
}

// -- The `--tui` plane graph screen (the third TUI graph attempt) ---------
//
// A true 2D nested layout: expanded namespaces render as actual boxes
// containing their children, laid out in char space by
// `crate::graph::plane`, with dependency edges routed by
// `crate::graph::plane_edges`. Unlike the rail view's single gutter or the
// canvas view's horizontal Sugiyama bands (both flatten the tree and funnel
// every edge into a narrow channel), this view spreads the graph across
// both dimensions the way the GUI's own nested-cluster rendering does --
// see `crate::graph::plane`'s module doc for why band-based layouts were
// rejected in real use.
//
// Paint order matters and is the reason garbling is structurally
// impossible: edges are painted into the grid first, then every box
// border, then every leaf/collapsed-namespace label -- each layer painted
// strictly *over* whatever the previous layer left in the same cells (see
// `draw_plane_graph`). An edge is only ever visible in the blank routing
// space `crate::graph::plane::shelf_pack` leaves between siblings and shelf
// rows; it can never visually collide with a box or a label because
// whichever was painted later always wins that cell.

/// Everything the plane screen needs, built fresh each frame from `App`
/// state -- mirrors [`CanvasView`]'s own "recompute, don't cache" precedent
/// (see [`rail_view`]'s module doc for why that's cheap enough to do every
/// frame): the pure nested layout, and the edges already routed against it.
pub struct PlaneView {
    layout: PlaneLayout,
    edges: PlaneEdges,
}

/// Build [`PlaneView`] from `app`'s current fold state: [`plane::layout`]
/// over `app.graph`/`app.layers`/`app.fold_collapsed`, with
/// [`plane_leaf_label`] supplying each leaf/collapsed row's exact text (so
/// this view's widths and content agree with the rail/canvas views' own
/// badges), then [`plane_edges::route_edges`] over the result using
/// [`rail_view::collapse_edges`]'s fold-aware edge list -- the same edge set
/// [`build_canvas_view`] routes.
pub fn build_plane_view(app: &App) -> PlaneView {
    let layout = plane::layout(&app.graph, &app.layers, &app.fold_collapsed, |id| {
        plane_leaf_label(app, id)
    });
    let edges = rail_view::collapse_edges(&app.graph, &app.graph.edges, &app.fold_collapsed);
    let routed = plane_edges::route_edges(&layout, &edges, &app.focus);
    PlaneView {
        layout,
        edges: routed,
    }
}

/// The styled spans for `id`'s label row: [`node_line`] for a plain drawn
/// module, or [`collapsed_row_spans`] (via [`rail_view::namespace_stats`])
/// when `id` is currently folded -- the same two span-builders the rail/
/// canvas views already use, so all three views render identical text/
/// badges for the same node. Unlike [`plain_row_text`] (which takes a
/// [`RailRow`] the caller already resolved via [`rail_view::visible_rows_with_layers`]),
/// this takes a bare id directly: [`crate::graph::plane::layout`]'s nested
/// walk discovers collapsed/leaf ids on its own by walking
/// [`crate::graph::model::ModuleNode::children`], never by consulting the
/// rail view's flattened row list at all (see that module's own doc).
fn plane_leaf_spans(app: &App, id: &NodeId) -> Vec<Span<'static>> {
    if app.fold_collapsed.contains(id) {
        let (module_count, changed_count) = rail_view::namespace_stats(&app.graph, id);
        collapsed_row_spans(app, id, module_count, changed_count)
    } else {
        node_line(app, id).spans
    }
}

/// The plain-text content of [`plane_leaf_spans`] -- what [`build_plane_view`]
/// feeds [`plane::layout`] as each row's label width.
fn plane_leaf_label(app: &App, id: &NodeId) -> String {
    plane_leaf_spans(app, id)
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// The `(layers, rows)` pair [`crate::core::focus::move_focus`] needs for
/// plane-mode spatial `h`/`j`/`k`/`l` -- [`plane::focus_grid`] over a freshly
/// built [`PlaneView`]'s layout, mirroring [`canvas_focus_grid`]'s own role
/// for the canvas view exactly.
pub fn plane_focus_grid(app: &App) -> (Vec<Vec<NodeId>>, plane::FocusRows) {
    let view = build_plane_view(app);
    plane::focus_grid(&view.layout)
}

/// `focus`'s own rect in `view`'s absolute char space, or `None` if `focus`
/// isn't present as a visible row at all -- what `crate::tui::event_loop`
/// feeds into [`clamp_scroll`]/[`clamp_scroll_x`] every frame for the
/// plane view's 2D auto-pan, mirroring [`focus_canvas_line`]/
/// [`focused_slot_range`]'s combined role for the canvas view.
pub fn focused_plane_rect(view: &PlaneView, focus: &NodeId) -> Option<plane::Rect> {
    view.layout.rows.get(focus).copied()
}

/// The plane view's total rendered height -- what `crate::tui::event_loop`
/// feeds into [`clamp_scroll`] as `total_rows`, mirroring
/// [`canvas_line_count`]'s role for the canvas view.
pub fn plane_view_height(view: &PlaneView) -> usize {
    view.layout.height
}

/// One grid cell's paint state while [`draw_plane_graph`] composes the full
/// (unscrolled, but height-windowed -- see that function's doc) char canvas:
/// the glyph to show and the style to paint it with. Distinct from
/// [`CanvasCell`]-style sparse storage -- the plane view's paint order
/// (edges, then box borders, then labels, each strictly overwriting the
/// last) is most naturally expressed as a dense per-visible-row buffer.
type PlaneCell = (char, Style);

fn empty_row(width: usize) -> Vec<PlaneCell> {
    vec![(' ', Style::default()); width]
}

/// Paint `spans`' characters into `row` starting at column `start_x`,
/// clipping silently at `row`'s own length -- used for both a leaf/
/// collapsed row's label ([`draw_plane_graph`]) and a box's title line
/// ([`plane_box_border_lines`]).
fn paint_spans(row: &mut [PlaneCell], start_x: usize, spans: &[Span<'static>]) {
    let mut cols = start_x..;
    for span in spans {
        for ch in span.content.chars() {
            let col = cols.next().expect("unbounded range always yields");
            if col < row.len() {
                row[col] = (ch, span.style);
            }
        }
    }
}

/// Paint a bare (unstyled-content, uniformly `style`d) string into `row`
/// starting at column `start_x`, clipping silently at `row`'s own length --
/// [`plane_box_border_lines`]'s own border-glyph painter.
fn paint_str(row: &mut [PlaneCell], start_x: usize, text: &str, style: Style) {
    for (col, ch) in (start_x..).zip(text.chars()) {
        if col < row.len() {
            row[col] = (ch, style);
        }
    }
}

/// Dim color for a box's own border glyphs -- matches [`RAIL_DIM`]'s "keep
/// a dense screen readable" role, just applied to plane-view box borders
/// instead of rail-gutter cells.
const BOX_BORDER_DIM: Color = Color::DarkGray;

/// `box_rect`'s title border text: `"╭─ Name ─...─╮"`, exactly `box_rect.w`
/// characters wide (padding with extra `─` before the closing `╮`, or
/// truncating in the pathological case a caller-supplied name is somehow
/// wider than [`crate::graph::plane`]'s own `title_min_w` reserved for it --
/// defensive, not expected to actually trigger given that invariant).
fn plane_box_title(name: &str, width: usize) -> String {
    let prefix = format!("\u{256d}\u{2500} {name} ");
    let prefix_len = prefix.chars().count();
    let closing = '\u{256e}';
    if prefix_len + 1 > width {
        let mut truncated: String = prefix.chars().take(width.saturating_sub(1)).collect();
        truncated.push(closing);
        return truncated;
    }
    let dashes = width - prefix_len - 1;
    format!("{prefix}{}{closing}", "\u{2500}".repeat(dashes))
}

/// `box_rect`'s bottom border: `"╰─...─╯"`, exactly `width` characters wide.
fn plane_box_bottom(width: usize) -> String {
    if width < 2 {
        return "\u{2570}".repeat(width);
    }
    format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(width - 2))
}

/// Compose the plane view's full char canvas as styled [`Line`]s, windowed
/// to the visible `[scroll_y, scroll_y + area.height)` row range up front
/// (so a graph thousands of rows tall never allocates more than one
/// screen's worth of grid rows -- see the module doc's sparse-space
/// efficiency note) and `[scroll_x, scroll_x + area.width)` column range via
/// [`slice_line_columns`] at the very end, exactly like [`draw_canvas_graph`]'s
/// own scroll-then-slice discipline. Returns [`PlaneEdges::hidden`] so
/// [`draw`] can feed it into [`draw_legend`]'s "+N edges hidden" hint, the
/// plane view's counterpart to [`Channel::dropped`]/[`rails::RailLayout::dropped_edges`].
fn draw_plane_graph(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    scroll_y: usize,
    scroll_x: usize,
) -> usize {
    let view = build_plane_view(app);
    if view.layout.rows.is_empty() && view.layout.boxes.is_empty() {
        frame.render_widget(
            Paragraph::new("(no visible nodes)").alignment(Alignment::Center),
            area,
        );
        return view.edges.hidden;
    }

    let total_height = view.layout.height;
    let width = view.layout.width.max(area.width as usize + scroll_x);
    let start_y = scroll_y.min(total_height);
    let end_y = (start_y + area.height as usize).min(total_height);
    let window_height = end_y.saturating_sub(start_y);

    let mut grid: Vec<Vec<PlaneCell>> = (0..window_height).map(|_| empty_row(width)).collect();

    // 1. Edges, painted first -- everything below overwrites them, exactly
    // the "edges pass under boxes" invariant `crate::graph::plane_edges`'s
    // module doc relies on.
    for cell in &view.edges.cells {
        if cell.y >= start_y && cell.y < end_y && cell.x < width {
            let row = &mut grid[cell.y - start_y];
            row[cell.x] = (
                cell.glyph,
                Style::default().fg(canvas_role_color(cell.role)),
            );
        }
    }

    // 2. Every box's own border, any order (boxes never overlap each other
    // -- see `crate::graph::plane`'s no-overlap guarantee).
    for (id, rect) in &view.layout.boxes {
        let name = app
            .graph
            .node(id)
            .map(|n| n.display_name.clone())
            .unwrap_or_else(|| id.to_string());
        let border_style = Style::default().fg(BOX_BORDER_DIM);
        if rect.y >= start_y && rect.y < end_y {
            let row = &mut grid[rect.y - start_y];
            paint_str(row, rect.x, &plane_box_title(&name, rect.w), border_style);
        }
        let bottom = rect.y + rect.h - 1;
        if bottom >= start_y && bottom < end_y {
            let row = &mut grid[bottom - start_y];
            paint_str(row, rect.x, &plane_box_bottom(rect.w), border_style);
        }
        for y in (rect.y + 1)..(rect.y + rect.h - 1) {
            if y >= start_y && y < end_y {
                let row = &mut grid[y - start_y];
                paint_str(row, rect.x, "\u{2502}", border_style);
                if rect.w > 1 {
                    paint_str(row, rect.x + rect.w - 1, "\u{2502}", border_style);
                }
            }
        }
    }

    // 3. Every leaf/collapsed row's label, on top of everything -- rows
    // never overlap a box's own border/padding ring (see
    // `crate::graph::plane`'s containment guarantee), only its blank
    // interior, so this never clobbers a border glyph.
    for (id, rect) in &view.layout.rows {
        if rect.y < start_y || rect.y >= end_y {
            continue;
        }
        let mut spans = plane_leaf_spans(app, id);
        if id == &app.focus {
            for span in &mut spans {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
        }
        let row = &mut grid[rect.y - start_y];
        paint_spans(row, rect.x, &spans);
    }

    let lines: Vec<Line> = grid
        .into_iter()
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut current: Option<(String, Style)> = None;
            for (ch, style) in row {
                match &mut current {
                    Some((text, cur_style)) if *cur_style == style => text.push(ch),
                    _ => {
                        if let Some((text, cur_style)) = current.take() {
                            spans.push(Span::styled(text, cur_style));
                        }
                        current = Some((ch.to_string(), style));
                    }
                }
            }
            if let Some((text, cur_style)) = current {
                spans.push(Span::styled(text, cur_style));
            }
            slice_line_columns(&Line::from(spans), scroll_x, area.width as usize)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
    view.edges.hidden
}

fn draw_file_view(frame: &mut Frame, area: Rect, file_view: &FileViewState) {
    let Some(file) = file_view.current_file() else {
        frame.render_widget(
            Paragraph::new("(no files)").alignment(Alignment::Center),
            area,
        );
        return;
    };
    let visible_rows = file_view_visible_rows(area.height);
    let start = file_view.scroll_row;
    let end = (start + visible_rows).min(file.lines.len());

    let mut lines: Vec<Line> = Vec::with_capacity(end.saturating_sub(start));
    for (offset, text) in file.lines[start..end].iter().enumerate() {
        let row = start + offset;
        let changed = file
            .changed_ranges
            .iter()
            .any(|&(s, e)| row >= s && row <= e);
        let mut spans = highlight::highlight_line(&file.path, text);
        if changed {
            for span in &mut spans {
                span.style = span.style.bg(Color::Rgb(40, 40, 0));
            }
        }
        lines.push(Line::from(spans));
    }

    let deleted_marker = if file.deleted { " (deleted)" } else { "" };
    let title = format!(" {}{deleted_marker} ", file.path.display());
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_diff(frame: &mut Frame, area: Rect, app: &App) {
    let Some(diff) = &app.diff else {
        frame.render_widget(
            Paragraph::new("Loading diff...").alignment(Alignment::Center),
            area,
        );
        return;
    };
    let Some(file) = diff.current_file() else {
        frame.render_widget(
            Paragraph::new("(no files)").alignment(Alignment::Center),
            area,
        );
        return;
    };

    let title = format!(
        " {} ({}/{}) ",
        file.path.display(),
        diff.file_index + 1,
        diff.files.len()
    );

    match diff.mode {
        DiffMode::Unified => draw_diff_unified(frame, area, &title, diff),
        DiffMode::SideBySide => draw_diff_side_by_side(frame, area, &title, diff),
    }
}

/// One rendered row of a unified/side-by-side diff: a gutter marker plus
/// the line text, colored by change kind. Shared by both diff render modes.
fn diff_line(marker: &str, text: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(color)),
        Span::raw(text.to_string()),
    ])
}

fn draw_diff_unified(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    diff: &crate::core::diff_state::DiffPaneState,
) {
    use crate::diffing::hunks::LinePair;
    let Some(file) = diff.current_file() else {
        return;
    };
    let mut lines = Vec::new();
    for hunk in &file.diff.hunks {
        for pair in &hunk.lines {
            let line = match *pair {
                LinePair::Unchanged { head, .. } => diff_line(
                    " ",
                    file.diff
                        .head_lines
                        .get(head as usize)
                        .map(String::as_str)
                        .unwrap_or(""),
                    Color::Gray,
                ),
                LinePair::Added { head } => diff_line(
                    "+",
                    file.diff
                        .head_lines
                        .get(head as usize)
                        .map(String::as_str)
                        .unwrap_or(""),
                    Color::Green,
                ),
                LinePair::Removed { base } => diff_line(
                    "-",
                    file.diff
                        .base_lines
                        .get(base as usize)
                        .map(String::as_str)
                        .unwrap_or(""),
                    Color::Red,
                ),
                LinePair::Changed { head, .. } => diff_line(
                    "~",
                    file.diff
                        .head_lines
                        .get(head as usize)
                        .map(String::as_str)
                        .unwrap_or(""),
                    Color::Yellow,
                ),
            };
            lines.push(line);
        }
    }
    let skip = diff.scroll_row.min(lines.len());
    let visible: Vec<Line> = lines.into_iter().skip(skip).collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    frame.render_widget(
        Paragraph::new(visible)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_diff_side_by_side(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    diff: &crate::core::diff_state::DiffPaneState,
) {
    use crate::diffing::hunks::LinePair;
    let Some(file) = diff.current_file() else {
        return;
    };
    let cols = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let mut base_lines = Vec::new();
    let mut head_lines = Vec::new();
    for hunk in &file.diff.hunks {
        for pair in &hunk.lines {
            match *pair {
                LinePair::Unchanged { base, head } => {
                    base_lines.push(diff_line(
                        " ",
                        file.diff
                            .base_lines
                            .get(base as usize)
                            .map(String::as_str)
                            .unwrap_or(""),
                        Color::Gray,
                    ));
                    head_lines.push(diff_line(
                        " ",
                        file.diff
                            .head_lines
                            .get(head as usize)
                            .map(String::as_str)
                            .unwrap_or(""),
                        Color::Gray,
                    ));
                }
                LinePair::Added { head } => {
                    base_lines.push(Line::from(""));
                    head_lines.push(diff_line(
                        "+",
                        file.diff
                            .head_lines
                            .get(head as usize)
                            .map(String::as_str)
                            .unwrap_or(""),
                        Color::Green,
                    ));
                }
                LinePair::Removed { base } => {
                    base_lines.push(diff_line(
                        "-",
                        file.diff
                            .base_lines
                            .get(base as usize)
                            .map(String::as_str)
                            .unwrap_or(""),
                        Color::Red,
                    ));
                    head_lines.push(Line::from(""));
                }
                LinePair::Changed { base, head } => {
                    base_lines.push(diff_line(
                        "~",
                        file.diff
                            .base_lines
                            .get(base as usize)
                            .map(String::as_str)
                            .unwrap_or(""),
                        Color::Yellow,
                    ));
                    head_lines.push(diff_line(
                        "~",
                        file.diff
                            .head_lines
                            .get(head as usize)
                            .map(String::as_str)
                            .unwrap_or(""),
                        Color::Yellow,
                    ));
                }
            }
        }
    }
    let skip = diff.scroll_row.min(base_lines.len());
    let base_visible: Vec<Line> = base_lines.into_iter().skip(skip).collect();
    let head_visible: Vec<Line> = head_lines.into_iter().skip(skip).collect();

    let base_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{title} (base)"));
    let head_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{title} (head)"));
    frame.render_widget(
        Paragraph::new(base_visible)
            .block(base_block)
            .wrap(Wrap { trim: false }),
        cols[0],
    );
    frame.render_widget(
        Paragraph::new(head_visible)
            .block(head_block)
            .wrap(Wrap { trim: false }),
        cols[1],
    );
}

/// The bottom legend/status strip: the keymap hint row plus review
/// progress -- the terminal equivalent of the GUI's floating hint overlay,
/// rendered as plain text since there's no floating-window concept in a
/// terminal grid. `notice`, when `Some`, replaces the hint line for this
/// frame rather than appending a third line -- see [`draw`]'s doc for why
/// the TUI needs a notice mechanism at all in place of `eprintln!`.
/// `dropped_edges` (from [`draw_rail_graph`]'s return) appends a `+N edges`
/// hint when the rail gutter's width cap dropped some -- see
/// [`crate::graph::rails::compute`]'s doc.
fn draw_legend(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    notice: Option<&str>,
    dropped_edges: usize,
    view_mode: ViewMode,
) {
    let hint = match notice {
        Some(notice) => notice.to_string(),
        None => match (app.screen, app.pane) {
            (Screen::Graph, Pane::Graph) => {
                let mut hint = match view_mode {
                    ViewMode::Rail => {
                        "` plane  j/k move  h/l fold/unfold  gd/gr follow deps  Enter open  d diff  t tests  v review  c comment  gt test  Ctrl-e edit  q quit"
                            .to_string()
                    }
                    ViewMode::Canvas => {
                        "` rail  h/j/k/l move  zc/zo fold/unfold  gd/gr follow deps  Enter open  d diff  t tests  v review  c comment  gt test  Ctrl-e edit  q quit"
                            .to_string()
                    }
                    ViewMode::Plane => {
                        "` canvas  h/j/k/l move  zc/zo fold/unfold  gd/gr follow deps  Enter open  d diff  t tests  v review  c comment  gt test  Ctrl-e edit  q quit"
                            .to_string()
                    }
                };
                if dropped_edges > 0 {
                    match view_mode {
                        ViewMode::Rail => hint
                            .push_str(&format!("  (+{dropped_edges} edges hidden, gutter capped)")),
                        ViewMode::Canvas => {
                            hint.push_str(&format!("  (+{dropped_edges} edges not drawn)"))
                        }
                        ViewMode::Plane => hint.push_str(&format!(
                            "  (+{dropped_edges} edges hidden — move focus to reveal)"
                        )),
                    }
                }
                hint
            }
            (Screen::Graph, Pane::File) => {
                "j/k scroll  Ctrl-d/u half-page  gg/G top/bottom  ]c/[c change  ]f/[f file  Ctrl-e edit  d diff  Esc back"
                    .to_string()
            }
            (Screen::Diff, _) => "j/k scroll  ]c/[c hunk  ]f/[f file  s toggle mode  Esc back".to_string(),
        },
    };
    let (reviewed, total) = app.review_progress();
    let status = format!("{reviewed}/{total} changed modules reviewed");
    let block = Block::default().borders(Borders::ALL).title(" vdiff ");
    let text = vec![Line::from(hint), Line::from(status)];
    frame.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

/// The floating j/k/Enter/Esc edge-picker overlay for `gd`/`gr` when a node
/// has more than one candidate -- the terminal equivalent of the GUI's
/// centered `egui::Window`, drawn as a centered fixed-size block on top of
/// everything else already painted this frame.
fn draw_picker(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };
    let width = area.width.min(50);
    let height = (picker.candidates.len() as u16 + 2).min(area.height).max(3);
    let popup = centered_rect(area, width, height);

    let items: Vec<ListItem> = picker
        .candidates
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let name = app
                .graph
                .node(id)
                .map(|n| n.display_name.as_str())
                .unwrap_or("?");
            let style = if i == picker.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Span::styled(name.to_string(), style))
        })
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Jump to"));
    frame.render_widget(list, popup);
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::app::EdgePicker;
    use crate::graph::layout::layout;
    use crate::graph::model::{DepEdge, DepKind, FileRef, ModuleNode, ProjectGraph};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    /// `leaf` depends on `target`; both are drawn (real, non-synthetic)
    /// nodes -- enough to exercise the rail graph's edge rendering in both
    /// directions.
    fn graph_fixture() -> ProjectGraph {
        let leaf = NodeId::from("leaf");
        let target = NodeId::from("target");

        let node = |id: &NodeId, name: &str, status: GitStatus| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: None,
            children: vec![],
            status,
            files: vec![FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };

        let mut nodes = HashMap::new();
        nodes.insert(leaf.clone(), node(&leaf, "leaf", GitStatus::Modified));
        nodes.insert(
            target.clone(),
            node(&target, "target", GitStatus::Unchanged),
        );

        ProjectGraph {
            roots: vec![leaf.clone(), target.clone()],
            nodes,
            edges: vec![DepEdge {
                from: leaf,
                to: target,
                kind: DepKind::Use,
            }],
        }
    }

    fn app_at(focus: &str) -> App {
        let graph = graph_fixture();
        let result = layout(&graph);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        App {
            graph,
            layers: result.layers,
            rows,
            focus: NodeId::from(focus),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 1,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        }
    }

    /// Render `app` to an 80x24 [`TestBackend`] at `rail_scroll` and
    /// flatten the resulting buffer to a single string (row-major, no
    /// separators) for substring assertions -- headless, no real terminal
    /// involved.
    fn render_to_string(app: &App) -> String {
        render_to_string_at(app, 80, 24, 0)
    }

    /// Like [`render_to_string`], but at an arbitrary terminal size and
    /// scroll offset -- used by tests exercising the rail view at both the
    /// 80x24 and 200x50 sizes the task brief calls out, and by scroll tests.
    fn render_to_string_at(app: &App, width: u16, height: u16, rail_scroll: usize) -> String {
        render_impl(app, width, height, None, rail_scroll)
    }

    fn render_impl(
        app: &App,
        width: u16,
        height: u16,
        notice: Option<&str>,
        rail_scroll: usize,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| draw(frame, app, notice, rail_scroll, 0, 0, ViewMode::Rail))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn rail_graph_shows_every_visible_row() {
        let app = app_at("leaf");
        let text = render_to_string(&app);
        assert!(text.contains("leaf"), "focused node name missing");
        assert!(text.contains("target"), "dependency name missing");
    }

    #[test]
    fn rail_graph_draws_a_rail_between_dependent_rows() {
        let app = app_at("leaf");
        let text = render_to_string(&app);
        assert!(
            text.contains('╮') || text.contains('╯'),
            "expected a rail connector glyph somewhere in the gutter, got: {text}"
        );
    }

    #[test]
    fn rail_graph_renders_at_a_wide_terminal_size_too() {
        let app = app_at("leaf");
        let text = render_to_string_at(&app, 200, 50, 0);
        assert!(text.contains("leaf"));
        assert!(text.contains("target"));
    }

    #[test]
    fn focused_row_gets_a_marker() {
        let app = app_at("leaf");
        let text = render_to_string(&app);
        assert!(text.contains('▸'), "expected the focus marker glyph");
    }

    #[test]
    fn reviewed_node_line_is_dimmed() {
        let mut app = app_at("leaf");
        app.reviewed.insert(NodeId::from("leaf"));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test backend");
        terminal
            .draw(|frame| draw(frame, &app, None, 0, 0, 0, ViewMode::Rail))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let dimmed = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                let cell = &buffer[(x, y)];
                cell.symbol() == "l"
                    && cell
                        .style()
                        .add_modifier
                        .contains(ratatui::style::Modifier::DIM)
            })
        });
        assert!(dimmed, "expected the reviewed node's line to carry DIM");
    }

    /// Real-use shape: two distinct drawn nodes (`elixir:Foo.Auction`,
    /// `elixir:Bar.Auction`) whose `display_name`s both collapse to plain
    /// `"Auction"`. The rail view must render each qualified with its
    /// parent's name rather than showing the same bare text twice -- see
    /// `crate::core::rail_view::disambiguated_labels`'s doc.
    #[test]
    fn rail_graph_disambiguates_two_nodes_that_share_a_display_name() {
        let foo_id = NodeId::from("elixir:Foo");
        let foo_auction_id = NodeId::from("elixir:Foo.Auction");
        let bar_id = NodeId::from("elixir:Bar");
        let bar_auction_id = NodeId::from("elixir:Bar.Auction");

        let leaf = |id: &NodeId, name: &str, parent: &NodeId| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: Some(parent.clone()),
            children: vec![],
            status: GitStatus::Modified,
            files: vec![FileRef {
                path: PathBuf::from(format!("{name}.ex")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };
        let namespace = |id: &NodeId, name: &str, child: &NodeId| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: None,
            children: vec![child.clone()],
            status: GitStatus::Unchanged,
            files: vec![],
        };

        let mut nodes = HashMap::new();
        nodes.insert(foo_id.clone(), namespace(&foo_id, "Foo", &foo_auction_id));
        nodes.insert(
            foo_auction_id.clone(),
            leaf(&foo_auction_id, "Auction", &foo_id),
        );
        nodes.insert(bar_id.clone(), namespace(&bar_id, "Bar", &bar_auction_id));
        nodes.insert(
            bar_auction_id.clone(),
            leaf(&bar_auction_id, "Auction", &bar_id),
        );
        let graph = ProjectGraph {
            roots: vec![foo_id, bar_id],
            nodes,
            edges: vec![],
        };

        let layers = crate::graph::layers::assign_layers(&graph);
        let result = layout(&graph);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        let app = App {
            graph,
            layers,
            rows,
            focus: foo_auction_id,
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 1,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        };
        let text = render_to_string(&app);
        assert!(
            text.contains("Foo.Auction"),
            "expected the Foo-side Auction row qualified with its parent, got: {text}"
        );
        assert!(
            text.contains("Bar.Auction"),
            "expected the Bar-side Auction row qualified with its parent, got: {text}"
        );
    }

    #[test]
    fn fold_collapsed_namespace_renders_a_summary_row() {
        let (graph, ns_id) = namespaced_graph_fixture();
        let layers = crate::graph::layers::assign_layers(&graph);
        let result = layout(&graph);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        let mut collapsed = HashSet::new();
        collapsed.insert(ns_id.clone());
        let app = App {
            graph,
            layers,
            rows,
            focus: ns_id,
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 1,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: collapsed,
        };
        let text = render_to_string(&app);
        assert!(text.contains("modules"), "expected the fold summary text");
    }

    /// A namespace `ns` with two drawn children `a`/`b` (no edges) -- for
    /// fold-rendering tests. Returns the graph plus the namespace's id.
    fn namespaced_graph_fixture() -> (ProjectGraph, NodeId) {
        let ns_id = NodeId::from("ns");
        let a_id = NodeId::from("a");
        let b_id = NodeId::from("b");
        let leaf = |id: &NodeId, name: &str| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: Some(ns_id.clone()),
            children: vec![],
            status: GitStatus::Modified,
            files: vec![FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };
        let mut nodes = HashMap::new();
        nodes.insert(a_id.clone(), leaf(&a_id, "a"));
        nodes.insert(b_id.clone(), leaf(&b_id, "b"));
        nodes.insert(
            ns_id.clone(),
            ModuleNode {
                id: ns_id.clone(),
                display_name: "ns".to_string(),
                parent: None,
                children: vec![a_id, b_id],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        (
            ProjectGraph {
                roots: vec![ns_id.clone()],
                nodes,
                edges: vec![],
            },
            ns_id,
        )
    }

    #[test]
    fn legend_shows_review_progress_and_hints() {
        let app = app_at("leaf");
        let text = render_to_string(&app);
        assert!(text.contains("reviewed"));
        assert!(text.contains("q quit"));
        assert!(text.contains("h/l fold/unfold"));
    }

    #[test]
    fn clamp_scroll_keeps_focus_within_margin() {
        // 20 rows, a 5-row viewport: focusing row 10 while scrolled to 0
        // must scroll forward so row 10 is visible with margin.
        let scroll = clamp_scroll(0, 10, 20, 5);
        assert!(scroll > 0);
        assert!(10 >= scroll && 10 < scroll + 5);
    }

    #[test]
    fn clamp_scroll_is_a_noop_when_focus_already_comfortably_visible() {
        let scroll = clamp_scroll(3, 6, 20, 10);
        assert_eq!(scroll, 3, "row 6 sits well inside [3+margin, 3+9-margin]");
    }

    #[test]
    fn clamp_scroll_adjusts_minimally_when_focus_sits_inside_the_margin() {
        // Margin is 2 here; focus at row 4 with scroll 3 is within the
        // top margin (rows 3/4 are the two margin rows), so scroll must
        // pull back just enough to restore the margin, not jump further.
        let scroll = clamp_scroll(3, 4, 20, 10);
        assert_eq!(scroll, 2);
    }

    #[test]
    fn clamp_scroll_never_scrolls_past_the_last_page() {
        let scroll = clamp_scroll(0, 19, 20, 10);
        assert_eq!(scroll, 10, "max_scroll = total_rows - viewport_height");
    }

    #[test]
    fn clamp_scroll_x_pans_right_to_reach_a_node_past_the_edge() {
        // A node at column 100 (width 8) with a 40-column viewport
        // scrolled to 0 must pan right so the node's whole width is
        // visible with margin.
        let scroll_x = clamp_scroll_x(0, 100, 8, 40);
        assert!(scroll_x > 0);
        assert!(100 >= scroll_x && 100 + 8 <= scroll_x + 40);
    }

    #[test]
    fn clamp_scroll_x_is_a_noop_when_the_focused_node_is_already_visible() {
        let scroll_x = clamp_scroll_x(10, 20, 5, 40);
        assert_eq!(scroll_x, 10);
    }

    #[test]
    fn clamp_scroll_x_pans_left_when_focus_moves_before_the_window() {
        let scroll_x = clamp_scroll_x(50, 10, 4, 40);
        assert!(scroll_x <= 10);
    }

    #[test]
    fn clamp_scroll_x_never_goes_negative() {
        let scroll_x = clamp_scroll_x(0, 0, 3, 40);
        assert_eq!(scroll_x, 0);
    }

    #[test]
    fn slice_line_columns_cuts_a_single_span_mid_content() {
        let line = Line::from(vec![Span::raw("hello world")]);
        let sliced = slice_line_columns(&line, 2, 5);
        let text: String = sliced.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "llo w");
    }

    #[test]
    fn slice_line_columns_preserves_style_across_multiple_spans() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let plain = Style::default();
        let line = Line::from(vec![
            Span::styled("abc".to_string(), bold),
            Span::styled("defgh".to_string(), plain),
        ]);
        // Window [2, 7): "c" from the bold span, "defg" from the plain one.
        let sliced = slice_line_columns(&line, 2, 5);
        assert_eq!(sliced.spans.len(), 2);
        assert_eq!(sliced.spans[0].content.as_ref(), "c");
        assert_eq!(sliced.spans[0].style, bold);
        assert_eq!(sliced.spans[1].content.as_ref(), "defg");
        assert_eq!(sliced.spans[1].style, plain);
    }

    #[test]
    fn slice_line_columns_drops_spans_entirely_outside_the_window() {
        let line = Line::from(vec![
            Span::raw("aaaa"),
            Span::raw("bbbb"),
            Span::raw("cccc"),
        ]);
        // Window [4, 8) is exactly the middle span.
        let sliced = slice_line_columns(&line, 4, 4);
        let text: String = sliced.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "bbbb");
    }

    #[test]
    fn slice_line_columns_is_unicode_safe_by_char_count() {
        // Each of these is one *character* but more than one byte --
        // slicing by byte offset would panic or cut mid-codepoint.
        let line = Line::from(vec![Span::raw("é€文abc")]);
        let sliced = slice_line_columns(&line, 1, 3);
        let text: String = sliced.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "€文a");
    }

    #[test]
    fn slice_line_columns_past_the_end_yields_an_empty_line() {
        let line = Line::from(vec![Span::raw("short")]);
        let sliced = slice_line_columns(&line, 100, 10);
        assert!(sliced.spans.is_empty());
    }

    #[test]
    fn clamp_scroll_is_zero_when_everything_fits() {
        let scroll = clamp_scroll(5, 3, 8, 20);
        assert_eq!(scroll, 0);
    }

    #[test]
    fn rail_visible_rows_matches_terminal_rows_minus_legend_height() {
        assert_eq!(rail_visible_rows(30), (30 - LEGEND_HEIGHT) as usize);
    }

    #[test]
    fn rail_visible_rows_never_zero_on_a_tiny_terminal() {
        assert_eq!(rail_visible_rows(0), 1);
    }

    #[test]
    fn picker_overlay_renders_candidates_when_open() {
        let mut app = app_at("leaf");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target")],
            selected: 0,
        });
        let text = render_to_string(&app);
        assert!(text.contains("Jump to"));
        assert!(text.contains("target"));
    }

    #[test]
    fn diff_screen_shows_loading_message_before_load_completes() {
        let mut app = app_at("leaf");
        app.screen = Screen::Diff;
        let text = render_to_string(&app);
        assert!(text.contains("Loading diff"));
    }

    #[test]
    fn diff_screen_unified_renders_added_and_removed_lines() {
        use crate::core::diff_state::{DiffPaneState, FileEntry};
        use crate::diffing::hunks::{DiffHunk, FileDiff, LinePair};

        let mut app = app_at("leaf");
        app.screen = Screen::Diff;
        app.diff = Some(DiffPaneState::new(
            NodeId::from("leaf"),
            vec![FileEntry {
                path: PathBuf::from("leaf.rs"),
                diff: FileDiff {
                    hunks: vec![DiffHunk {
                        lines: vec![LinePair::Removed { base: 0 }, LinePair::Added { head: 0 }],
                    }],
                    base_lines: vec!["old line".to_string()],
                    head_lines: vec!["new line".to_string()],
                },
            }],
        ));
        let text = render_to_string(&app);
        assert!(text.contains("old line"));
        assert!(text.contains("new line"));
    }

    #[test]
    fn file_view_screen_renders_file_lines() {
        use crate::core::file_view::FileViewEntry;

        let mut app = app_at("leaf");
        app.pane = Pane::File;
        app.file_view = Some(FileViewState::new(
            NodeId::from("leaf"),
            vec![FileViewEntry {
                path: PathBuf::from("leaf.rs"),
                lines: vec!["fn main() {}".to_string()],
                changed_ranges: vec![(0, 0)],
                deleted: false,
            }],
        ));
        let text = render_to_string(&app);
        assert!(text.contains("fn main"));
        assert!(text.contains("leaf.rs"));
    }

    #[test]
    fn file_view_visible_rows_never_zero_even_on_a_tiny_terminal() {
        assert_eq!(file_view_visible_rows(0), 1);
        let reserved = LEGEND_HEIGHT + FILE_HEADER_HEIGHT + 2;
        assert_eq!(file_view_visible_rows(30), (30 - reserved) as usize);
    }

    // -- Fix: file-less rows never panic if they do render (review
    // feedback) ------------------------------------------------------------
    //
    // `crate::core::app::open_file`/`open_diff` now no-op on a file-less
    // node (a collapsed namespace row's own id), so `App::file_view`/
    // `App::diff` should never actually end up holding a zero-files
    // `FileViewState`/`DiffPaneState` in practice. These two tests pin down
    // the belt-and-suspenders case anyway: IF one ever did get through
    // (a future caller forgetting the guard, a stale state from before this
    // fix, ...), rendering it must degrade gracefully to the existing
    // "(no files)" placeholder rather than panicking on an empty
    // `files`/`current_file()`.

    #[test]
    fn file_view_screen_with_zero_files_renders_the_no_files_placeholder_without_panicking() {
        let mut app = app_at("leaf");
        app.pane = Pane::File;
        app.file_view = Some(FileViewState::new(NodeId::from("ns"), vec![]));
        let text = render_to_string(&app);
        assert!(text.contains("(no files)"));
    }

    #[test]
    fn diff_screen_with_zero_files_renders_the_no_files_placeholder_without_panicking() {
        use crate::core::diff_state::DiffPaneState;
        let mut app = app_at("leaf");
        app.screen = Screen::Diff;
        app.diff = Some(DiffPaneState::new(NodeId::from("ns"), vec![]));
        let text = render_to_string(&app);
        assert!(text.contains("(no files)"));
    }

    // -- Fix: band separators break the scroll math (review feedback) -----
    //
    // `draw_rail_graph` used to slice its *row* list by
    // `rail_scroll..rail_scroll + area.height` and insert a band separator
    // line wherever the layer changed within that window -- so the
    // rendered line count could exceed `area.height` whenever more than
    // one layer transition fell inside the visible window, and
    // `ratatui::Paragraph` silently clipped the overflow off the bottom.
    // `build_display_lines`/`focus_display_line`/`display_line_count` fix
    // this by scrolling/clamping in the same (row + separator) line space
    // `draw_rail_graph` actually renders in.

    /// A strict chain `n0 -> n1 -> ... -> n{count-1}`, each node its own
    /// layer (longest-path layering puts a linear chain one node per layer
    /// -- see `crate::graph::layers`' own tests) -- enough layer
    /// transitions packed into a small viewport to reproduce the clipping
    /// bug: with a 4-line viewport, a naive row-based slice can need up to
    /// 4 separators *plus* 4 rows (8 lines) to render 4 rows' worth of
    /// content.
    fn chain_graph(count: usize) -> ProjectGraph {
        let names: Vec<String> = (0..count).map(|i| format!("n{i}")).collect();
        let mut nodes = HashMap::new();
        for name in &names {
            nodes.insert(
                NodeId::from(name.as_str()),
                ModuleNode {
                    id: NodeId::from(name.as_str()),
                    display_name: name.clone(),
                    parent: None,
                    children: vec![],
                    status: GitStatus::Unchanged,
                    files: vec![FileRef {
                        path: PathBuf::from(format!("{name}.rs")),
                        base_blob: Some("b".to_string()),
                        head_blob: Some("h".to_string()),
                    }],
                },
            );
        }
        let edges: Vec<DepEdge> = names
            .windows(2)
            .map(|pair| DepEdge {
                from: NodeId::from(pair[0].as_str()),
                to: NodeId::from(pair[1].as_str()),
                kind: DepKind::Use,
            })
            .collect();
        ProjectGraph {
            roots: names.iter().map(|n| NodeId::from(n.as_str())).collect(),
            nodes,
            edges,
        }
    }

    fn app_for_chain(graph: ProjectGraph, focus: &str) -> App {
        let result = layout(&graph);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        App {
            graph,
            layers: result.layers,
            rows,
            focus: NodeId::from(focus),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 1,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        }
    }

    #[test]
    fn build_display_lines_inserts_one_separator_per_layer_transition() {
        let graph = chain_graph(3);
        let rows = crate::core::rail_view::visible_rows_with_layers(
            &graph,
            &layout(&graph).layers,
            &HashSet::new(),
        );
        // 3 layers, each with one row: separator, row, separator, row,
        // separator, row = 6 display lines.
        assert_eq!(display_line_count(&rows), 6);
    }

    #[test]
    fn focus_display_line_counts_separators_ahead_of_the_focused_row() {
        let graph = chain_graph(3);
        let layers = layout(&graph).layers;
        let rows =
            crate::core::rail_view::visible_rows_with_layers(&graph, &layers, &HashSet::new());
        // n0 is display-line index 1 (after its own layer's separator);
        // n2 is display-line index 5 (three separators + two earlier rows
        // ahead of it).
        assert_eq!(focus_display_line(&rows, &NodeId::from("n0")), Some(1));
        assert_eq!(focus_display_line(&rows, &NodeId::from("n2")), Some(5));
    }

    #[test]
    fn focus_display_line_none_when_focus_is_not_visible() {
        let graph = chain_graph(2);
        let layers = layout(&graph).layers;
        let rows =
            crate::core::rail_view::visible_rows_with_layers(&graph, &layers, &HashSet::new());
        assert_eq!(focus_display_line(&rows, &NodeId::from("ghost")), None);
    }

    /// Reproduces the clipping bug end to end: 6 single-node layers (12
    /// display lines total: 6 separators + 6 rows), a 4-line viewport, and
    /// focus on the very last row -- the row-index-only scroll math this
    /// fixes would compute a scroll offset that, once separators are
    /// actually rendered, clips the focused row's line off the bottom.
    #[test]
    fn focused_row_stays_visible_at_the_bottom_of_a_deep_chain() {
        let graph = chain_graph(6);
        let layers = layout(&graph).layers;
        let rows =
            crate::core::rail_view::visible_rows_with_layers(&graph, &layers, &HashSet::new());
        let focus = NodeId::from("n5");
        let focus_idx = focus_display_line(&rows, &focus).expect("n5 must be visible");
        let total_lines = display_line_count(&rows);
        let viewport_height = 4;
        let scroll = clamp_scroll(0, focus_idx, total_lines, viewport_height);

        let app = app_for_chain(graph, "n5");
        // area.height == viewport_height once LEGEND_HEIGHT is subtracted.
        let text = render_to_string_at(&app, 40, viewport_height as u16 + LEGEND_HEIGHT, scroll);
        assert!(
            text.contains("n5"),
            "focused row must still be on screen, got:\n{text}"
        );
    }

    /// Companion to the previous test: after scrolling down to reveal the
    /// bottom row, moving focus back to the top must scroll back up so the
    /// top row is visible again (and not, say, get stuck at the previous
    /// bottom-anchored scroll offset).
    #[test]
    fn focused_row_stays_visible_after_scrolling_back_to_the_top() {
        let graph = chain_graph(6);
        let layers = layout(&graph).layers;
        let rows =
            crate::core::rail_view::visible_rows_with_layers(&graph, &layers, &HashSet::new());
        let total_lines = display_line_count(&rows);
        let viewport_height = 4;

        let bottom_focus_idx = focus_display_line(&rows, &NodeId::from("n5")).unwrap();
        let scrolled_down = clamp_scroll(0, bottom_focus_idx, total_lines, viewport_height);
        assert!(
            scrolled_down > 0,
            "sanity: the chain is taller than the viewport"
        );

        let top_focus_idx = focus_display_line(&rows, &NodeId::from("n0")).unwrap();
        let scroll = clamp_scroll(scrolled_down, top_focus_idx, total_lines, viewport_height);

        let app = app_for_chain(graph, "n0");
        let text = render_to_string_at(&app, 40, viewport_height as u16 + LEGEND_HEIGHT, scroll);
        assert!(
            text.contains("n0"),
            "focused row must be back on screen after scrolling up, got:\n{text}"
        );
    }

    // -- The canvas graph screen (issue #17) --------------------------------

    /// `p1`/`p2` both depend on `child` -- a diamond top spread across one
    /// band, converging on a single node in the next.
    fn diamond_graph_fixture() -> ProjectGraph {
        let p1 = NodeId::from("p1");
        let p2 = NodeId::from("p2");
        let child = NodeId::from("child");
        let node = |id: &NodeId, name: &str| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Modified,
            files: vec![FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };
        let mut nodes = HashMap::new();
        nodes.insert(p1.clone(), node(&p1, "p1"));
        nodes.insert(p2.clone(), node(&p2, "p2"));
        nodes.insert(child.clone(), node(&child, "child"));
        ProjectGraph {
            roots: vec![p1.clone(), p2.clone(), child.clone()],
            nodes,
            edges: vec![
                DepEdge {
                    from: p1,
                    to: child.clone(),
                    kind: DepKind::Use,
                },
                DepEdge {
                    from: p2,
                    to: child,
                    kind: DepKind::Use,
                },
            ],
        }
    }

    fn app_for(graph: ProjectGraph, focus: &str) -> App {
        let layers = crate::graph::layers::assign_layers(&graph);
        App {
            graph,
            layers,
            rows: vec![],
            focus: NodeId::from(focus),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 1,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        }
    }

    #[test]
    fn canvas_view_lays_out_a_diamond_with_two_bands() {
        let app = app_for(diamond_graph_fixture(), "child");
        let view = build_canvas_view(&app);
        assert_eq!(view.layout.bands.len(), 2);
        assert_eq!(view.layout.bands[0].len(), 2, "p1/p2 share the top band");
        assert_eq!(
            view.layout.bands[1].len(),
            1,
            "child alone in the next band"
        );
        assert_eq!(view.channels.len(), 1);
    }

    #[test]
    fn canvas_graph_renders_both_parent_names_and_the_child_and_a_bend() {
        let app = app_for(diamond_graph_fixture(), "child");
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("test backend");
        terminal
            .draw(|frame| draw(frame, &app, None, 0, 0, 0, ViewMode::Canvas))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("p1"), "missing p1:\n{text}");
        assert!(text.contains("p2"), "missing p2:\n{text}");
        assert!(text.contains("child"), "missing child:\n{text}");
        assert!(
            text.contains('╮') || text.contains('╯') || text.contains('│'),
            "expected at least one routed channel glyph, got:\n{text}"
        );
    }

    #[test]
    fn canvas_scroll_x_pans_the_rendered_content_instead_of_only_clipping_it() {
        // A narrow viewport: at `canvas_scroll_x = 0`, the leftmost label
        // ("p1", column 0 in the diamond fixture) is visible and a label
        // further right ("child") is clipped off. Panning right should
        // flip that -- "p1" scrolls out of view and "child" becomes
        // reachable, proving this is real horizontal scrolling and not
        // just a wider unconditional clip.
        let app = app_for(diamond_graph_fixture(), "child");
        let width = 6u16;
        let height = 20u16;

        let mut terminal_at_zero = Terminal::new(TestBackend::new(width, height)).expect("test");
        terminal_at_zero
            .draw(|frame| draw(frame, &app, None, 0, 0, 0, ViewMode::Canvas))
            .expect("draw");
        let text_at_zero = buffer_text(terminal_at_zero.backend().buffer());
        assert!(text_at_zero.contains("p1"), "p1 should be visible unpanned");

        let mut terminal_panned = Terminal::new(TestBackend::new(width, height)).expect("test");
        terminal_panned
            .draw(|frame| draw(frame, &app, None, 0, 0, 20, ViewMode::Canvas))
            .expect("draw");
        let text_panned = buffer_text(terminal_panned.backend().buffer());
        assert!(
            !text_panned.contains("p1"),
            "p1 should have panned out of view:\n{text_panned}"
        );
        assert_ne!(
            text_at_zero, text_panned,
            "panning must actually change what's rendered"
        );
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn canvas_legend_advertises_the_view_toggle_and_fold_chord() {
        let app = app_for(diamond_graph_fixture(), "child");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test backend");
        terminal
            .draw(|frame| draw(frame, &app, None, 0, 0, 0, ViewMode::Canvas))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(text.contains('`'), "expected the view-toggle hint");
        assert!(text.contains("zc/zo"), "expected the fold-chord hint");
    }

    #[test]
    fn canvas_legend_shows_edges_not_drawn_when_a_channel_drops_edges() {
        let dropped_edges = 3;
        let app = app_for(diamond_graph_fixture(), "child");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_legend(frame, area, &app, None, dropped_edges, ViewMode::Canvas)
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        // The legend's `Paragraph` wraps at the terminal width, which can
        // split the hint's own words across lines -- assert on the
        // fragments the wrap can't separate rather than the exact
        // contiguous phrase.
        assert!(
            text.contains("+3 edges"),
            "expected the dropped-edge count, got: {text}"
        );
        assert!(
            text.contains("not"),
            "expected the canvas-specific wording, got: {text}"
        );
        assert!(
            text.contains("drawn"),
            "expected the canvas-specific wording, got: {text}"
        );
    }

    #[test]
    fn empty_canvas_shows_a_placeholder_without_panicking() {
        let app = app_for(
            ProjectGraph {
                roots: vec![],
                nodes: HashMap::new(),
                edges: vec![],
            },
            "nobody",
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("test backend");
        terminal
            .draw(|frame| draw(frame, &app, None, 0, 0, 0, ViewMode::Canvas))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(text.contains("no visible nodes"));
    }

    #[test]
    fn focus_canvas_line_and_canvas_line_count_agree_with_the_rendered_band() {
        let app = app_for(diamond_graph_fixture(), "child");
        let view = build_canvas_view(&app);
        let focus_line = focus_canvas_line(&view, &NodeId::from("child")).expect("child visible");
        // `child`'s band is the second one, after the top band's own label
        // line plus the channel between them.
        assert_eq!(focus_line, 1 + view.channels[0].height);
        assert!(canvas_line_count(&view) > focus_line);
    }

    #[test]
    fn canvas_focus_grid_matches_move_focus_over_the_diamond() {
        let app = app_for(diamond_graph_fixture(), "p1");
        let (layers, rows) = canvas_focus_grid(&app);
        let target = crate::core::focus::move_focus(
            &layers,
            &rows,
            &NodeId::from("p1"),
            crate::core::focus::Direction::Down,
        );
        assert_eq!(target, NodeId::from("child"));
    }

    // -- The plane graph screen --------------------------------------------

    fn app_for_plane(graph: ProjectGraph, focus: &str) -> App {
        app_for(graph, focus)
    }

    fn render_plane_to_string(
        app: &App,
        width: u16,
        height: u16,
        scroll_y: usize,
        scroll_x: usize,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
        terminal
            .draw(|frame| draw(frame, app, None, 0, scroll_y, scroll_x, ViewMode::Plane))
            .expect("draw");
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn plane_graph_renders_a_nested_box_with_its_children_and_labels() {
        let (graph, _ns_id) = namespaced_graph_fixture();
        let app = app_for_plane(graph, "a");
        let text = render_plane_to_string(&app, 80, 24, 0, 0);
        assert!(text.contains("ns"), "missing namespace title, got:\n{text}");
        assert!(text.contains('a'), "missing child a, got:\n{text}");
        assert!(text.contains('b'), "missing child b, got:\n{text}");
        assert!(
            text.contains('\u{256d}') && text.contains('\u{256e}'),
            "expected the box's top border corners, got:\n{text}"
        );
        assert!(
            text.contains('\u{2570}') && text.contains('\u{256f}'),
            "expected the box's bottom border corners, got:\n{text}"
        );
    }

    #[test]
    fn plane_graph_bolds_the_focused_node_s_own_cell() {
        let (graph, _ns_id) = namespaced_graph_fixture();
        let app = app_for_plane(graph, "a");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test backend");
        terminal
            .draw(|frame| draw(frame, &app, None, 0, 0, 0, ViewMode::Plane))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let bolded = (0..buffer.area.height).any(|y| {
            (0..buffer.area.width).any(|x| {
                let cell = &buffer[(x, y)];
                cell.symbol() == "a"
                    && cell
                        .style()
                        .add_modifier
                        .contains(ratatui::style::Modifier::BOLD)
            })
        });
        assert!(bolded, "expected the focused node's label to carry BOLD");
    }

    #[test]
    fn plane_graph_vertical_panning_changes_rendered_content() {
        let (graph, _ns_id) = namespaced_graph_fixture();
        let app = app_for_plane(graph, "a");
        let view = build_plane_view(&app);
        let total_height = plane_view_height(&view);
        assert!(total_height > 1, "fixture must have more than one row");

        let text_at_top = render_plane_to_string(&app, 80, 8, 0, 0);
        let text_scrolled = render_plane_to_string(&app, 80, 8, total_height.saturating_sub(1), 0);
        assert_ne!(
            text_at_top, text_scrolled,
            "vertical scroll must change what's rendered"
        );
    }

    #[test]
    fn plane_graph_horizontal_panning_changes_rendered_content() {
        let (graph, _ns_id) = namespaced_graph_fixture();
        let app = app_for_plane(graph, "a");
        let view = build_plane_view(&app);
        let total_width = view.layout.width;
        assert!(total_width > 1, "fixture must have more than one column");

        let text_at_left = render_plane_to_string(&app, 6, 24, 0, 0);
        let text_panned = render_plane_to_string(&app, 6, 24, 0, total_width.saturating_sub(1));
        assert_ne!(
            text_at_left, text_panned,
            "horizontal scroll must change what's rendered"
        );
    }

    #[test]
    fn plane_graph_draws_an_edge_between_dependent_columns() {
        let app = app_for_plane(diamond_graph_fixture(), "child");
        let text = render_plane_to_string(&app, 80, 24, 0, 0);
        assert!(text.contains("p1"), "missing p1, got:\n{text}");
        assert!(text.contains("p2"), "missing p2, got:\n{text}");
        assert!(text.contains("child"), "missing child, got:\n{text}");
        assert!(
            text.contains('\u{2502}')
                || text.contains('\u{256e}')
                || text.contains('\u{256f}')
                || text.contains('\u{253c}'),
            "expected at least one routed edge glyph, got:\n{text}"
        );
    }

    #[test]
    fn plane_edges_route_between_the_correct_absolute_columns() {
        let app = app_for_plane(diamond_graph_fixture(), "child");
        let view = build_plane_view(&app);
        let child_rect = view
            .layout
            .rows
            .get(&NodeId::from("child"))
            .expect("child is a row");
        // At least one edge cell should land in `child`'s own column range
        // (its incoming edges converge on it).
        let touches_child_column = view.edges.cells.iter().any(|cell| {
            cell.x >= child_rect.x && cell.x < child_rect.x + child_rect.w.max(1)
                || cell.x == child_rect.x_center().round() as usize
        });
        assert!(
            touches_child_column,
            "expected a routed edge cell in child's column range"
        );
    }

    #[test]
    fn plane_legend_shows_edges_hidden_hint_when_the_budget_trips() {
        let dropped_edges = 5;
        let app = app_for_plane(diamond_graph_fixture(), "child");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test backend");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_legend(frame, area, &app, None, dropped_edges, ViewMode::Plane)
            })
            .expect("draw");
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("+5 edges"),
            "expected the hidden-edge count, got: {text}"
        );
        assert!(
            text.contains("hidden") || text.contains("reveal"),
            "expected the plane-specific wording, got: {text}"
        );
    }

    #[test]
    fn empty_plane_shows_a_placeholder_without_panicking() {
        let app = app_for_plane(
            ProjectGraph {
                roots: vec![],
                nodes: HashMap::new(),
                edges: vec![],
            },
            "nobody",
        );
        let text = render_plane_to_string(&app, 40, 10, 0, 0);
        assert!(text.contains("no visible nodes"));
    }

    #[test]
    fn plane_focus_grid_matches_move_focus_over_the_diamond() {
        // Each of `p1`/`p2`/`child` is a small top-level leaf, and
        // `crate::graph::plane::shelf_pack`'s target-width heuristic wraps
        // onto a new shelf row well before three items this size would
        // share one -- so each ends up its own row here, ordered `p1`,
        // `p2`, `child` (layer 0 before layer 1, `p1` before `p2` by name).
        // `Direction::Down` (`j`) steps between adjacent rows, landing on
        // whichever's x-nearest -- with one entry per row, that's simply
        // the next one in sequence.
        let app = app_for_plane(diamond_graph_fixture(), "p1");
        let (layers, rows) = plane_focus_grid(&app);
        let target = crate::core::focus::move_focus(
            &layers,
            &rows,
            &NodeId::from("p1"),
            crate::core::focus::Direction::Down,
        );
        assert_eq!(target, NodeId::from("p2"));
    }
}
