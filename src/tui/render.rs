//! Pure(-ish) `ratatui` rendering for the focused-neighborhood view (issue
//! #16): the focused module plus its direct dependencies and dependents,
//! one frame of the call-stack story at a time -- the nix-tree pattern,
//! not a ported graph canvas (see the module's own top-level doc for why).
//! Every function here takes `&core::App`/a `&mut Frame` and paints;
//! nothing here mutates `App` or performs IO, so this is exercised entirely
//! through `ratatui::backend::TestBackend` in this module's tests, with no
//! real terminal involved.

use ratatui::layout::{Alignment, Constraint, Direction as LayoutDirection, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::core::app::{App, Pane, Screen};
use crate::core::diff_state::DiffMode;
use crate::core::file_view::FileViewState;
use crate::core::focus::{dep_targets, dependent_sources};
use crate::graph::model::{GitStatus, NodeId};
use crate::review::findings::Severity;
use crate::tui::highlight;

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

/// Paint one frame for the current `app` state: the graph screen (focused
/// neighborhood or file pane, per [`App::pane`]) or the full-screen diff
/// pane, per [`App::screen`], plus the bottom legend strip and any open
/// edge-picker overlay. `notice`, when set, takes over the legend strip's
/// hint line for this one frame -- see `crate::tui::TuiState::notice`'s doc
/// for why the TUI needs this display-only glue state at all (in short:
/// `eprintln!` is invisible/garbled while the alternate screen owns the
/// terminal).
pub fn draw(frame: &mut Frame, app: &App, notice: Option<&str>) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(LEGEND_HEIGHT)])
        .split(area);
    let (main_area, legend_area) = (chunks[0], chunks[1]);

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
                draw_neighborhood(frame, main_area, app);
            }
        }
        Screen::Diff => draw_diff(frame, main_area, app),
    }

    draw_legend(frame, legend_area, app, notice);

    if app.pane == Pane::Graph {
        draw_picker(frame, area, app);
    }
}

/// One node's rendered line: a status-colored bullet, its display name, and
/// trailing badges -- changed-test checkmark, findings count/severity,
/// comment count, reviewed mark. Shared by every column in
/// [`draw_neighborhood`] so the badge set never drifts between them.
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

/// The nix-tree-style neighborhood view: dependents (things that call into
/// the focused node) above, the focused node itself in the middle, direct
/// dependencies (what the focused node calls into) below -- one frame of
/// the call-stack story at a time, not the whole graph. A layer breadcrumb
/// ("Layer i/n") sits in the focused block's title.
fn draw_neighborhood(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Length(5),
            Constraint::Percentage(35),
        ])
        .split(area);
    let (dependents_area, focus_area, deps_area) = (rows[0], rows[1], rows[2]);

    let dependents = dependent_sources(&app.graph, &app.focus);
    let deps = dep_targets(&app.graph, &app.focus);

    draw_node_list(frame, dependents_area, "Called by", &dependents, app);
    draw_focused_block(frame, focus_area, app);
    draw_node_list(frame, deps_area, "Calls", &deps, app);
}

fn draw_node_list(frame: &mut Frame, area: Rect, title: &str, ids: &[NodeId], app: &App) {
    let items: Vec<ListItem> = if ids.is_empty() {
        vec![ListItem::new(Span::styled(
            "(none)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        ids.iter()
            .map(|id| ListItem::new(node_line(app, id)))
            .collect()
    };
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

fn draw_focused_block(frame: &mut Frame, area: Rect, app: &App) {
    let breadcrumb = layer_breadcrumb(&app.layers, &app.focus);
    let title = format!(" Focused -- {breadcrumb} ");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().add_modifier(Modifier::BOLD));
    let line = node_line(app, &app.focus);
    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// `"Layer i/n"`, 1-based, for whichever layer of `layers` contains
/// `focus`, or `"Layer ?"` if it isn't in any (shouldn't happen for a
/// focusable node, but this is render-time display, not a navigation
/// invariant, so it degrades gracefully rather than panicking).
fn layer_breadcrumb(layers: &[Vec<NodeId>], focus: &NodeId) -> String {
    match layers.iter().position(|layer| layer.contains(focus)) {
        Some(idx) => format!("Layer {}/{}", idx + 1, layers.len()),
        None => "Layer ?".to_string(),
    }
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
fn draw_legend(frame: &mut Frame, area: Rect, app: &App, notice: Option<&str>) {
    let hint = match notice {
        Some(notice) => notice.to_string(),
        None => match (app.screen, app.pane) {
            (Screen::Graph, Pane::Graph) => {
                "h/j/k/l move  gd/gr follow deps  Enter open  d diff  t tests  v review  c comment  gt test  Ctrl-e edit  q quit"
                    .to_string()
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
    /// nodes -- enough to exercise the neighborhood view's "called by"/
    /// "calls" columns in both directions.
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

    /// Render `app` to an 80x24 [`TestBackend`] and flatten the resulting
    /// buffer to a single string (row-major, no separators) for substring
    /// assertions -- headless, no real terminal involved.
    fn render_to_string(app: &App) -> String {
        render_to_string_with_notice(app, None)
    }

    /// Like [`render_to_string`], but with a legend-strip notice (see
    /// [`draw`]'s `notice` parameter) -- used by tests exercising
    /// `crate::tui::TuiState::notice`'s render side without needing the
    /// glue that sets it.
    pub(crate) fn render_to_string_with_notice(app: &App, notice: Option<&str>) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| draw(frame, app, notice))
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
    fn neighborhood_view_shows_focused_node_and_its_dependency() {
        let app = app_at("leaf");
        let text = render_to_string(&app);
        assert!(text.contains("Focused"), "focused block title missing");
        assert!(text.contains("leaf"), "focused node name missing");
        assert!(text.contains("Calls"), "deps column title missing");
        assert!(text.contains("target"), "dependency name missing");
        assert!(text.contains("Layer 1/"), "layer breadcrumb missing");
    }

    #[test]
    fn neighborhood_view_shows_dependents_column() {
        let app = app_at("target");
        let text = render_to_string(&app);
        assert!(
            text.contains("Called by"),
            "dependents column title missing"
        );
        assert!(text.contains("leaf"), "dependent name missing");
    }

    #[test]
    fn neighborhood_view_shows_none_placeholder_with_no_edges() {
        let app = app_at("target");
        let text = render_to_string(&app);
        assert!(text.contains("(none)"), "target has no outgoing deps");
    }

    #[test]
    fn legend_shows_review_progress_and_hints() {
        let app = app_at("leaf");
        let text = render_to_string(&app);
        assert!(text.contains("reviewed"));
        assert!(text.contains("q quit"));
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
}
