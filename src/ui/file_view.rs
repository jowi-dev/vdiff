//! File viewer pane rendering: the focused node's source, virtualized via
//! `egui_extras::TableBuilder` and syntax-highlighted the same way
//! [`crate::ui::diff_view`] highlights diff rows -- a two-column gutter
//! (line number, plus a change marker for lines inside a
//! [`FileViewEntry::changed_ranges`] span) and code table, pinned to the
//! top of `scroll_row` (unlike `diff_view`'s row-centering scroll, so
//! `scroll_row` reads literally as "top visible line" for `j`/`k` vim
//! scrolling). Painted inside [`crate::ui::overlay`]'s fullscreen editor
//! overlay, below its own header strip -- this module no longer draws a
//! header or a focus border of its own (there's only ever one place this
//! can be showing now, and the overlay's header covers the same
//! information plus the focused node's identity).
use egui::{Align, Color32, Context, TextStyle, Ui};
use egui_extras::syntax_highlighting::{highlight, CodeTheme};
use egui_extras::{Column, TableBuilder};

use crate::core::file_view::{FileViewEntry, FileViewState};
use crate::review::findings::{findings_at_line, Finding};
use crate::ui::diff_view::language_for;
use crate::ui::theme;

/// Accent color for the change-marker bar drawn in the gutter next to lines
/// inside a changed range.
const CHANGE_MARKER_COLOR: Color32 = Color32::from_rgb(0x6a, 0xc9, 0x6a);

/// Draw the file viewer's current file: virtualized table, key-hint
/// footer. `node_findings` is every AI review finding (issue #5) attached
/// to the node this pane is showing (see [`crate::core::app::App::findings_for`]) --
/// each with a `line` gets a small severity-colored gutter marker (hover
/// for the summary); line-less (file/node-level) findings have nothing to
/// mark here and only show up in the graph badge/focus overlay. Returns
/// the number of table rows that fit in the space available this frame --
/// the eframe glue feeds this back into
/// [`crate::core::app::App::viewport_rows`] for `Ctrl-d`/`Ctrl-u`
/// half-page math, since only the render layer knows the actual pixel
/// height available.
pub fn show(ui: &mut Ui, file_view: &FileViewState, node_findings: &[Finding]) -> usize {
    let Some(file) = file_view.current_file() else {
        ui.heading("No files for this node");
        footer(ui);
        return 1;
    };

    let ctx = ui.ctx().clone();
    let theme = CodeTheme::from_memory(&ctx, ui.style());
    let highlight = HighlightCtx {
        ctx: &ctx,
        theme: &theme,
        language: language_for(&file.path),
    };
    let row_height = ui.text_style_height(&TextStyle::Monospace);
    let viewport_rows = (ui.available_height() / row_height).floor().max(1.0) as usize;

    render_table(
        ui,
        &highlight,
        file,
        file_view.scroll_row,
        row_height,
        node_findings,
    );

    footer(ui);
    viewport_rows
}

/// The syntax-highlighting inputs [`render_table`]/[`code_cell`] need,
/// bundled so `render_table` stays under clippy's argument-count limit --
/// see [`crate::ui::diff_view`] for the same syntect pipeline used here.
struct HighlightCtx<'a> {
    ctx: &'a Context,
    theme: &'a CodeTheme,
    language: &'a str,
}

/// The one-line key-hint footer, matching this project's keyboard-first
/// convention.
fn footer(ui: &mut Ui) {
    ui.separator();
    ui.label(
        "j/k scroll   Ctrl-d/Ctrl-u half-page   gg/G top/bottom   ]c/[c change   ]f/[f file   Esc close",
    );
}

/// Whether `line` (0-based) falls inside any of `ranges`.
fn is_changed(ranges: &[(usize, usize)], line: usize) -> bool {
    ranges
        .iter()
        .any(|&(start, end)| line >= start && line <= end)
}

/// Virtualized two-column (gutter, code) table of every line in `file`,
/// pinned to `scroll_row`'s top.
fn render_table(
    ui: &mut Ui,
    highlight: &HighlightCtx,
    file: &FileViewEntry,
    scroll_row: usize,
    row_height: f32,
    node_findings: &[Finding],
) {
    let total_rows = file.lines.len();
    TableBuilder::new(ui)
        .column(Column::exact(64.0))
        .column(Column::remainder().at_least(80.0))
        .scroll_to_row(
            scroll_row.min(total_rows.saturating_sub(1)),
            Some(Align::TOP),
        )
        .body(|body| {
            body.rows(row_height, total_rows, |mut row_ui| {
                let idx = row_ui.index();
                let changed = is_changed(&file.changed_ranges, idx);
                // `Finding::line` is 1-based, matching the line number this
                // gutter cell shows -- `idx` (0-based, matching
                // `file.lines`) needs the same `+ 1` `is_changed`'s ranges
                // don't (those are already stored 0-based, see
                // `FileViewEntry::changed_ranges`'s doc).
                let line_findings = findings_at_line(node_findings, idx as u32 + 1);
                row_ui.col(|ui| {
                    gutter_cell(ui, idx, changed, &line_findings);
                });
                row_ui.col(|ui| {
                    code_cell(ui, highlight, &file.lines[idx]);
                });
            });
        });
}

/// A gutter cell: 1-based line number, dim, plus a colored bar on the left
/// edge when `changed`, plus -- when `line_findings` is non-empty (issue
/// #5) -- a small severity-colored dot after the line number, in the
/// highest severity present's color (see [`worst_severity`]), whose hover
/// text lists every finding at this line via [`format_finding_line`]. This
/// is deliberately the whole treatment: no click-through, no persistent
/// popup -- the issue's "keep it simple" guidance for the file pane.
fn gutter_cell(ui: &mut Ui, line: usize, changed: bool, line_findings: &[&Finding]) {
    if changed {
        let rect = ui.max_rect();
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
        ui.painter().rect_filled(bar, 0.0, CHANGE_MARKER_COLOR);
    }
    ui.horizontal(|ui| {
        ui.weak(format!("{}", line + 1));
        if let Some(severity) = worst_severity(line_findings) {
            let hover_text = line_findings
                .iter()
                .map(|f| crate::review::findings::format_finding_line(f))
                .collect::<Vec<_>>()
                .join("\n");
            ui.colored_label(theme::severity_color(severity), "\u{25cf}")
                .on_hover_text(hover_text);
        }
    });
}

/// The highest severity present among `findings`, or `None` if empty --
/// [`gutter_cell`]'s own tiny wrapper over
/// [`crate::review::findings::worst_severity`], which takes `&[Finding]`
/// rather than `&[&Finding]` (what [`findings_at_line`] returns).
fn worst_severity(findings: &[&Finding]) -> Option<crate::review::findings::Severity> {
    findings.iter().map(|f| f.severity).max()
}

/// A code cell: `text` syntax-highlighted via the same syntect pipeline
/// [`crate::ui::diff_view`] uses.
fn code_cell(ui: &mut Ui, highlight_ctx: &HighlightCtx, text: &str) {
    let job = highlight(
        highlight_ctx.ctx,
        ui.style(),
        highlight_ctx.theme,
        text,
        highlight_ctx.language,
    );
    ui.add(egui::Label::new(job).selectable(false));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_changed_true_within_range_inclusive() {
        let ranges = vec![(2, 4), (10, 10)];
        assert!(!is_changed(&ranges, 1));
        assert!(is_changed(&ranges, 2));
        assert!(is_changed(&ranges, 3));
        assert!(is_changed(&ranges, 4));
        assert!(!is_changed(&ranges, 5));
        assert!(is_changed(&ranges, 10));
    }

    #[test]
    fn is_changed_false_with_no_ranges() {
        assert!(!is_changed(&[], 0));
    }

    fn finding(severity: crate::review::findings::Severity) -> Finding {
        Finding {
            node_id: Some("n".to_string()),
            path: None,
            line: Some(1),
            severity,
            summary: "some finding".to_string(),
            detail: None,
        }
    }

    #[test]
    fn worst_severity_none_for_no_findings() {
        assert_eq!(worst_severity(&[]), None);
    }

    #[test]
    fn worst_severity_picks_the_highest_present() {
        use crate::review::findings::Severity;
        let low = finding(Severity::Low);
        let high = finding(Severity::High);
        assert_eq!(worst_severity(&[&low, &high]), Some(Severity::High));
    }
}
