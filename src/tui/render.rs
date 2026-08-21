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

use ratatui::layout::{Alignment, Constraint, Direction as LayoutDirection, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::core::app::{App, Pane, Screen};
use crate::core::diff_state::DiffMode;
use crate::core::file_view::FileViewState;
use crate::core::rail_view::{self, RailRow};
use crate::graph::model::{GitStatus, NodeId};
use crate::graph::rails::{self, RailRole};
use crate::review::findings::Severity;
use crate::tui::highlight;

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
/// `rail_scroll` is the rail view's current scroll offset (row index of the
/// topmost visible row), already clamped by the caller via [`clamp_scroll`]
/// -- see [`rail_visible_rows`]'s doc for why that clamping happens in
/// `crate::tui::event_loop` rather than in here.
pub fn draw(frame: &mut Frame, app: &App, notice: Option<&str>, rail_scroll: usize) {
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
                dropped_edges = draw_rail_graph(frame, main_area, app, rail_scroll);
            }
        }
        Screen::Diff => draw_diff(frame, main_area, app),
    }

    draw_legend(frame, legend_area, app, notice, dropped_edges);

    if app.pane == Pane::Graph {
        draw_picker(frame, area, app);
    }
}

/// How many rail-view data rows fit in a terminal of `terminal_rows` total
/// rows -- the rail area fills everything above the legend strip, with no
/// border/header of its own (unlike [`file_view_visible_rows`]'s file pane,
/// the rail view has no per-pane chrome eating into it, so this is just
/// [`LEGEND_HEIGHT`] subtracted). Shared between [`draw`] (which must
/// actually render that many rows) and `crate::tui::event_loop` (which
/// feeds this into [`clamp_scroll`] every frame, mirroring exactly how
/// `event_loop` already threads [`file_view_visible_rows`] into
/// `App::viewport_rows` before each `terminal.draw` call).
pub fn rail_visible_rows(terminal_rows: u16) -> usize {
    terminal_rows.saturating_sub(LEGEND_HEIGHT).max(1) as usize
}

/// Adjust `scroll` (the previous frame's topmost visible row index) by as
/// little as possible so `focus_idx` stays within [`SCROLL_MARGIN`] rows of
/// the viewport's top/bottom edge -- a scroll-margin policy (like `vim`'s
/// `scrolloff`), not a center-on-jump one: a `gd`/`gr` jump to a far row
/// still lands inside the margin rather than dead-center, but since this
/// runs fresh every frame from the current `scroll`/`focus_idx` regardless
/// of *why* focus moved, a far jump is still guaranteed visible -- no
/// special-casing needed for `gd`/`gr` versus plain `j`/`k`. Degrades
/// gracefully when `viewport_height` is too short to afford a full margin
/// on both ends (halves the margin rather than refusing to scroll at all).
/// Always returns a value in `0..=total_rows.saturating_sub(viewport_height)`.
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

/// One node's rendered line: a status-colored bullet, its display name, and
/// trailing badges -- changed-test checkmark, findings count/severity,
/// comment count, reviewed mark. Shared by every row [`row_line`] builds
/// for a [`RailRow::Node`], so the badge set stays exactly what the GUI's
/// own `crate::ui::graph_view::paint_node`/badge functions paint. A
/// reviewed node's entire line gets [`Modifier::DIM`] on top of its normal
/// colors -- the terminal analogue of the GUI's
/// `crate::ui::theme::dim_reviewed` (which blends a box's fill 1/3 toward
/// gray): a true partial color blend isn't expressible per-glyph in a
/// 16/256-color terminal palette, so `DIM` is the closest "still legible,
/// visibly muted" terminal equivalent.
fn node_line(app: &App, id: &NodeId) -> Line<'static> {
    let Some(node) = app.graph.node(id) else {
        return Line::from(id.to_string());
    };
    let mut spans = vec![
        Span::styled("● ", Style::default().fg(status_color(node.status))),
        Span::raw(node.display_name.clone()),
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
/// scrolled via `j`/`k`/`gd`/`gr`. Only `rail_scroll..rail_scroll +
/// area.height` rows are ever built into [`Line`]s -- the caller
/// (`crate::tui::event_loop`) has already clamped `rail_scroll` (see
/// [`clamp_scroll`]) so the focused row is guaranteed inside that window.
/// Returns the rail layout's `dropped_edges` count (`0` unless the gutter's
/// width cap kicked in -- see [`crate::graph::rails::compute`]'s doc) so
/// [`draw`] can pass it on to [`draw_legend`]'s `+N edges` hint.
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

    let start = rail_scroll.min(rows.len().saturating_sub(1));
    let end = (start + area.height as usize).min(rows.len());
    // The layer of the row just above the viewport (if any), so a band
    // separator isn't spuriously repainted at the very top of the viewport
    // for a layer that actually started further up, already scrolled past.
    let mut prev_layer = start.checked_sub(1).map(|i| rows[i].1);

    let mut lines: Vec<Line> = Vec::with_capacity(end - start);
    for (idx, (row, layer)) in rows.iter().enumerate().take(end).skip(start) {
        if prev_layer != Some(*layer) {
            lines.push(band_separator_line(*layer, area.width));
        }
        prev_layer = Some(*layer);

        let focused = row.id() == &app.focus;
        lines.push(row_line(app, row, &rail_layout, idx, focused));
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
fn row_line(
    app: &App,
    row: &RailRow,
    rail_layout: &rails::RailLayout,
    row_idx: usize,
    focused: bool,
) -> Line<'static> {
    let mut spans = gutter_spans(rail_layout, row_idx);
    spans.push(Span::raw(if focused { "▸ " } else { "  " }));
    match row {
        RailRow::Node(id) => spans.extend(node_line(app, id).spans),
        RailRow::Collapsed {
            namespace,
            module_count,
            changed_count,
        } => spans.extend(collapsed_row_spans(
            app,
            namespace,
            *module_count,
            *changed_count,
        )),
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
/// task brief's fold summary format.
fn collapsed_row_spans(
    app: &App,
    namespace: &NodeId,
    module_count: usize,
    changed_count: usize,
) -> Vec<Span<'static>> {
    let name = app
        .graph
        .node(namespace)
        .map(|n| n.display_name.clone())
        .unwrap_or_else(|| namespace.to_string());
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
) {
    let hint = match notice {
        Some(notice) => notice.to_string(),
        None => match (app.screen, app.pane) {
            (Screen::Graph, Pane::Graph) => {
                let mut hint = "j/k move  h/l fold/unfold  gd/gr follow deps  Enter open  d diff  t tests  v review  c comment  gt test  Ctrl-e edit  q quit"
                    .to_string();
                if dropped_edges > 0 {
                    hint.push_str(&format!("  (+{dropped_edges} edges hidden, gutter capped)"));
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
            .draw(|frame| draw(frame, app, notice, rail_scroll))
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
            .draw(|frame| draw(frame, &app, None, 0))
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
}
