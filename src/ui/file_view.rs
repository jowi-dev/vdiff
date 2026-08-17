//! File viewer pane rendering: the focused node's source, virtualized via
//! `egui_extras::TableBuilder` and syntax-highlighted the same way
//! [`crate::ui::diff_view`] highlights diff rows -- a two-column gutter
//! (line number, plus a change marker for lines inside a
//! [`FileViewEntry::changed_ranges`] span) and code table, pinned to the
//! top of `scroll_row` (unlike `diff_view`'s row-centering scroll, so
//! `scroll_row` reads literally as "top visible line" for `j`/`k` vim
//! scrolling).

use egui::{Align, Color32, Context, TextStyle, Ui};
use egui_extras::syntax_highlighting::{highlight, CodeTheme};
use egui_extras::{Column, TableBuilder};

use crate::core::file_view::{FileViewEntry, FileViewState};
use crate::ui::diff_view::language_for;
use crate::ui::theme;

/// Accent color for the change-marker bar drawn in the gutter next to lines
/// inside a changed range.
const CHANGE_MARKER_COLOR: Color32 = Color32::from_rgb(0x6a, 0xc9, 0x6a);

/// Draw the file viewer pane for `file_view`'s current file: an accent
/// border reflecting `focused` (see [`theme::pane_border_stroke`]), header,
/// virtualized table, key-hint footer. Returns the number of table rows
/// that fit in the space available this frame -- the eframe glue feeds
/// this back into [`crate::core::app::App::viewport_rows`] for
/// `Ctrl-d`/`Ctrl-u` half-page math, since only the render layer knows the
/// actual pixel height available.
pub fn show(ui: &mut Ui, file_view: &FileViewState, focused: bool) -> usize {
    let border_rect = ui.max_rect();
    ui.painter().rect_stroke(
        border_rect,
        0.0,
        theme::pane_border_stroke(focused),
        egui::StrokeKind::Inside,
    );

    let Some(file) = file_view.current_file() else {
        ui.heading("No files for this node");
        footer(ui);
        return 1;
    };

    header(ui, file_view, file);
    ui.separator();

    let ctx = ui.ctx().clone();
    let theme = CodeTheme::from_memory(&ctx, ui.style());
    let language = language_for(&file.path);
    let row_height = ui.text_style_height(&TextStyle::Monospace);
    let viewport_rows = (ui.available_height() / row_height).floor().max(1.0) as usize;

    render_table(
        ui,
        &ctx,
        &theme,
        language,
        file,
        file_view.scroll_row,
        row_height,
    );

    footer(ui);
    viewport_rows
}

/// The panel header: file path, `(i/N)` when multi-file, change-range
/// count, and a `(deleted)` marker for files absent at head.
fn header(ui: &mut Ui, file_view: &FileViewState, file: &FileViewEntry) {
    ui.horizontal(|ui| {
        ui.strong(file.path.display().to_string());
        if file.deleted {
            ui.weak("(deleted)");
        }
        if file_view.files.len() > 1 {
            ui.label(format!(
                "({}/{})",
                file_view.file_index + 1,
                file_view.files.len()
            ));
        }
        if !file.changed_ranges.is_empty() {
            ui.label(format!("{} changes", file.changed_ranges.len()));
        }
    });
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
    ctx: &Context,
    theme: &CodeTheme,
    language: &str,
    file: &FileViewEntry,
    scroll_row: usize,
    row_height: f32,
) {
    let total_rows = file.lines.len();
    TableBuilder::new(ui)
        .column(Column::exact(48.0))
        .column(Column::remainder().at_least(80.0))
        .scroll_to_row(
            scroll_row.min(total_rows.saturating_sub(1)),
            Some(Align::TOP),
        )
        .body(|body| {
            body.rows(row_height, total_rows, |mut row_ui| {
                let idx = row_ui.index();
                let changed = is_changed(&file.changed_ranges, idx);
                row_ui.col(|ui| {
                    gutter_cell(ui, idx, changed);
                });
                row_ui.col(|ui| {
                    code_cell(ui, ctx, theme, language, &file.lines[idx]);
                });
            });
        });
}

/// A gutter cell: 1-based line number, dim, plus a colored bar on the left
/// edge when `changed`.
fn gutter_cell(ui: &mut Ui, line: usize, changed: bool) {
    if changed {
        let rect = ui.max_rect();
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
        ui.painter().rect_filled(bar, 0.0, CHANGE_MARKER_COLOR);
    }
    ui.weak(format!("{}", line + 1));
}

/// A code cell: `text` syntax-highlighted via the same syntect pipeline
/// [`crate::ui::diff_view`] uses.
fn code_cell(ui: &mut Ui, ctx: &Context, theme: &CodeTheme, language: &str, text: &str) {
    let job = highlight(ctx, ui.style(), theme, text, language);
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
}
