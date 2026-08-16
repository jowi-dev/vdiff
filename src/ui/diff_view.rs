//! Diff pane rendering: virtualized rows via `egui_extras::TableBuilder`,
//! syntax-highlighted via `egui_extras::syntax_highlighting` (syntect),
//! background-tinted per line kind with intraline highlights layered over
//! `Changed` rows. Row-shaping (`display_rows`/`unified_rows`) is pure and
//! unit-tested; painting is thin, untested glue -- see the module doc on
//! [`crate::ui`] for that split's rationale.

use std::path::Path;

use egui::{Color32, Context, FontId, Rect, TextStyle, Ui};
use egui_extras::syntax_highlighting::{highlight, CodeTheme};
use egui_extras::{Column, TableBuilder};

use crate::core::diff_state::{DiffMode, DiffPaneState, FileEntry};
use crate::diffing::hunks::LinePair;
use crate::diffing::intraline::{intraline, HighlightSpan};

/// How a row should be tinted, mirroring [`LinePair`] without its payload
/// (the row already carries `base`/`head` line indices directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Unchanged,
    Added,
    Removed,
    Changed,
}

/// One side-by-side row: `base`/`head` line indices (either may be absent
/// for `Added`/`Removed`), plus intraline highlight spans for `Changed`
/// rows (empty otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayRow {
    pub kind: RowKind,
    pub base: Option<u32>,
    pub head: Option<u32>,
    pub base_spans: Vec<HighlightSpan>,
    pub head_spans: Vec<HighlightSpan>,
}

/// Map a hunk's [`LinePair`]s to side-by-side [`DisplayRow`]s, computing
/// intraline spans for `Changed` pairs against `base_lines`/`head_lines`.
/// Pure -- one row per line pair.
pub fn display_rows(
    lines: &[LinePair],
    base_lines: &[String],
    head_lines: &[String],
) -> Vec<DisplayRow> {
    lines
        .iter()
        .map(|line| match *line {
            LinePair::Unchanged { base, head } => DisplayRow {
                kind: RowKind::Unchanged,
                base: Some(base),
                head: Some(head),
                base_spans: Vec::new(),
                head_spans: Vec::new(),
            },
            LinePair::Added { head } => DisplayRow {
                kind: RowKind::Added,
                base: None,
                head: Some(head),
                base_spans: Vec::new(),
                head_spans: Vec::new(),
            },
            LinePair::Removed { base } => DisplayRow {
                kind: RowKind::Removed,
                base: Some(base),
                head: None,
                base_spans: Vec::new(),
                head_spans: Vec::new(),
            },
            LinePair::Changed { base, head } => {
                let (base_spans, head_spans) =
                    intraline(&base_lines[base as usize], &head_lines[head as usize]);
                DisplayRow {
                    kind: RowKind::Changed,
                    base: Some(base),
                    head: Some(head),
                    base_spans,
                    head_spans,
                }
            }
        })
        .collect()
}

/// A unified-mode row's gutter marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnifiedKind {
    Unchanged,
    Added,
    Removed,
}

/// One unified row: exactly one side's line, plus intraline spans if it's
/// half of a `Changed` pair (empty otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedRow {
    pub kind: UnifiedKind,
    pub base: Option<u32>,
    pub head: Option<u32>,
    pub spans: Vec<HighlightSpan>,
}

/// Map a hunk's [`LinePair`]s to unified rows: `Unchanged`/`Added`/
/// `Removed` map 1:1, but `Changed` splits into a `Removed` row (the base
/// line) followed by an `Added` row (the head line), each carrying that
/// side's intraline spans -- unified diffs show both sides of a rewritten
/// line, just sequentially instead of side-by-side. Pure.
pub fn unified_rows(
    lines: &[LinePair],
    base_lines: &[String],
    head_lines: &[String],
) -> Vec<UnifiedRow> {
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        match *line {
            LinePair::Unchanged { base, head } => rows.push(UnifiedRow {
                kind: UnifiedKind::Unchanged,
                base: Some(base),
                head: Some(head),
                spans: Vec::new(),
            }),
            LinePair::Added { head } => rows.push(UnifiedRow {
                kind: UnifiedKind::Added,
                base: None,
                head: Some(head),
                spans: Vec::new(),
            }),
            LinePair::Removed { base } => rows.push(UnifiedRow {
                kind: UnifiedKind::Removed,
                base: Some(base),
                head: None,
                spans: Vec::new(),
            }),
            LinePair::Changed { base, head } => {
                let (base_spans, head_spans) =
                    intraline(&base_lines[base as usize], &head_lines[head as usize]);
                rows.push(UnifiedRow {
                    kind: UnifiedKind::Removed,
                    base: Some(base),
                    head: None,
                    spans: base_spans,
                });
                rows.push(UnifiedRow {
                    kind: UnifiedKind::Added,
                    base: None,
                    head: Some(head),
                    spans: head_spans,
                });
            }
        }
    }
    rows
}

/// Background tint for a side-by-side/unified row kind, `None` for
/// `Unchanged` (no tint -- the canvas background shows through).
fn row_tint(kind: RowKind) -> Option<Color32> {
    match kind {
        RowKind::Unchanged => None,
        RowKind::Added => Some(Color32::from_rgba_unmultiplied(0x2e, 0x5c, 0x2e, 0x60)),
        RowKind::Removed => Some(Color32::from_rgba_unmultiplied(0x5c, 0x2a, 0x2a, 0x60)),
        RowKind::Changed => Some(Color32::from_rgba_unmultiplied(0x6b, 0x5c, 0x1f, 0x50)),
    }
}

/// Background tint for a [`UnifiedKind`], mirroring [`row_tint`].
fn unified_tint(kind: UnifiedKind) -> Option<Color32> {
    match kind {
        UnifiedKind::Unchanged => None,
        UnifiedKind::Added => Some(Color32::from_rgba_unmultiplied(0x2e, 0x5c, 0x2e, 0x60)),
        UnifiedKind::Removed => Some(Color32::from_rgba_unmultiplied(0x5c, 0x2a, 0x2a, 0x60)),
    }
}

/// Stronger tint for intraline highlight spans, painted over the row tint.
fn intraline_tint() -> Color32 {
    Color32::from_rgba_unmultiplied(0xff, 0xd0, 0x40, 0x55)
}

/// Draw the diff pane for `diff`'s current file: header, virtualized table
/// (side-by-side or unified per `diff.mode`), and key-hint footer.
pub fn show(ui: &mut Ui, diff: &DiffPaneState) {
    let Some(file) = diff.current_file() else {
        ui.heading("No files changed for this node");
        footer(ui);
        return;
    };

    header(ui, diff, file);
    ui.separator();

    let ctx = ui.ctx().clone();
    let theme = CodeTheme::from_memory(&ctx, ui.style());
    let language = language_for(&file.path);
    let row_height = ui.text_style_height(&TextStyle::Monospace);

    match diff.mode {
        DiffMode::SideBySide => render_side_by_side(
            ui,
            &ctx,
            &theme,
            language,
            file,
            diff.scroll_row,
            row_height,
        ),
        DiffMode::Unified => render_unified(
            ui,
            &ctx,
            &theme,
            language,
            file,
            diff.scroll_row,
            row_height,
        ),
    }

    footer(ui);
}

/// The panel header: file path, `M of N` file indicator, mode, hunk count.
fn header(ui: &mut Ui, diff: &DiffPaneState, file: &FileEntry) {
    ui.horizontal(|ui| {
        ui.strong(file.path.display().to_string());
        ui.label(format!(
            "{}/{}",
            diff.file_index + 1,
            diff.files.len().max(1)
        ));
        ui.label(match diff.mode {
            DiffMode::SideBySide => "side-by-side",
            DiffMode::Unified => "unified",
        });
        ui.label(format!("{} hunks", file.diff.hunks.len()));
    });
}

/// The one-line key-hint footer, matching this project's keyboard-first
/// convention.
fn footer(ui: &mut Ui) {
    ui.separator();
    ui.label("j/k scroll   ]c/[c hunk   ]f/[f file   s toggle mode   Esc back");
}

/// Every rendered row of `file`'s current hunks, flattened, tagged with
/// which hunk each came from (used only to decide whether the code below
/// needs a hunk grouping in the future -- currently unused beyond length).
fn render_side_by_side(
    ui: &mut Ui,
    ctx: &Context,
    theme: &CodeTheme,
    language: &str,
    file: &FileEntry,
    scroll_row: usize,
    row_height: f32,
) {
    let rows: Vec<DisplayRow> = file
        .diff
        .hunks
        .iter()
        .flat_map(|hunk| display_rows(&hunk.lines, &file.diff.base_lines, &file.diff.head_lines))
        .collect();

    TableBuilder::new(ui)
        .column(Column::exact(40.0))
        .column(Column::remainder().at_least(80.0))
        .column(Column::exact(40.0))
        .column(Column::remainder().at_least(80.0))
        .scroll_to_row(scroll_row.min(rows.len().saturating_sub(1)), None)
        .body(|body| {
            body.rows(row_height, rows.len(), |mut row_ui| {
                let row = &rows[row_ui.index()];
                row_ui.col(|ui| {
                    line_number_cell(ui, row_tint(row.kind), row.base);
                });
                row_ui.col(|ui| {
                    code_cell(
                        ui,
                        ctx,
                        theme,
                        language,
                        row_tint(row.kind),
                        row.base.map(|i| file.diff.base_lines[i as usize].as_str()),
                        &row.base_spans,
                    );
                });
                row_ui.col(|ui| {
                    line_number_cell(ui, row_tint(row.kind), row.head);
                });
                row_ui.col(|ui| {
                    code_cell(
                        ui,
                        ctx,
                        theme,
                        language,
                        row_tint(row.kind),
                        row.head.map(|i| file.diff.head_lines[i as usize].as_str()),
                        &row.head_spans,
                    );
                });
            });
        });
}

/// Unified-mode render: one column of `marker | line-no | code` per row.
/// `scroll_row` is reused as-is from [`crate::core::diff_state::DiffPaneState`],
/// which counts side-by-side rows -- unified mode can have more rows than
/// that (each `Changed` pair becomes two), so the scrolled-to position is
/// approximate after a mode toggle. Documented limitation, not a bug: hunk
/// navigation still lands correctly within either mode.
fn render_unified(
    ui: &mut Ui,
    ctx: &Context,
    theme: &CodeTheme,
    language: &str,
    file: &FileEntry,
    scroll_row: usize,
    row_height: f32,
) {
    let rows: Vec<UnifiedRow> = file
        .diff
        .hunks
        .iter()
        .flat_map(|hunk| unified_rows(&hunk.lines, &file.diff.base_lines, &file.diff.head_lines))
        .collect();

    TableBuilder::new(ui)
        .column(Column::exact(16.0))
        .column(Column::exact(40.0))
        .column(Column::remainder().at_least(80.0))
        .scroll_to_row(scroll_row.min(rows.len().saturating_sub(1)), None)
        .body(|body| {
            body.rows(row_height, rows.len(), |mut row_ui| {
                let row = &rows[row_ui.index()];
                let marker = match row.kind {
                    UnifiedKind::Unchanged => " ",
                    UnifiedKind::Added => "+",
                    UnifiedKind::Removed => "-",
                };
                let line_no = row.base.or(row.head);
                let text = row
                    .base
                    .map(|i| file.diff.base_lines[i as usize].as_str())
                    .or_else(|| row.head.map(|i| file.diff.head_lines[i as usize].as_str()));

                row_ui.col(|ui| {
                    if let Some(tint) = unified_tint(row.kind) {
                        ui.painter().rect_filled(ui.max_rect(), 0.0, tint);
                    }
                    ui.label(marker);
                });
                row_ui.col(|ui| {
                    line_number_cell(ui, unified_tint(row.kind), line_no);
                });
                row_ui.col(|ui| {
                    code_cell(
                        ui,
                        ctx,
                        theme,
                        language,
                        unified_tint(row.kind),
                        text,
                        &row.spans,
                    );
                });
            });
        });
}

/// A gutter cell showing `line` 1-based, or blank if `None` (the missing
/// side of an `Added`/`Removed` row).
fn line_number_cell(ui: &mut Ui, tint: Option<Color32>, line: Option<u32>) {
    if let Some(tint) = tint {
        ui.painter().rect_filled(ui.max_rect(), 0.0, tint);
    }
    if let Some(line) = line {
        ui.weak(format!("{}", line + 1));
    }
}

/// A code cell: `tint` painted as the row background, `spans` as a
/// stronger overlay (monospace character-width math -- see the module
/// doc), and `text` syntax-highlighted on top. Blank if `text` is `None`.
fn code_cell(
    ui: &mut Ui,
    ctx: &Context,
    theme: &CodeTheme,
    language: &str,
    tint: Option<Color32>,
    text: Option<&str>,
    spans: &[HighlightSpan],
) {
    if let Some(tint) = tint {
        ui.painter().rect_filled(ui.max_rect(), 0.0, tint);
    }
    let Some(text) = text else {
        return;
    };

    let font_id = TextStyle::Monospace.resolve(ui.style());
    paint_intraline_spans(ui, ui.max_rect(), text, spans, &font_id);

    let job = highlight(ctx, ui.style(), theme, text, language);
    ui.add(egui::Label::new(job).selectable(false));
}

/// Paint `spans`' byte ranges of `text` as [`intraline_tint`] rects within
/// `rect`, using a monospace character-width estimate (character count *
/// single-glyph advance) rather than exact text measurement -- fine for the
/// monospace font code cells use, approximate for any non-ASCII content.
fn paint_intraline_spans(
    ui: &Ui,
    rect: Rect,
    text: &str,
    spans: &[HighlightSpan],
    font_id: &FontId,
) {
    if spans.is_empty() {
        return;
    }
    let char_width = ui.ctx().fonts_mut(|fonts| fonts.glyph_width(font_id, 'M'));
    for span in spans {
        let prefix_chars = text[..span.start].chars().count() as f32;
        let span_chars = text[span.start..span.end].chars().count() as f32;
        let x0 = rect.min.x + prefix_chars * char_width;
        let width = span_chars * char_width;
        let span_rect =
            Rect::from_min_size(egui::pos2(x0, rect.min.y), egui::vec2(width, rect.height()));
        ui.painter().rect_filled(span_rect, 0.0, intraline_tint());
    }
}

/// File-extension-based language hint for [`highlight`]'s syntect lookup.
fn language_for(path: &Path) -> &str {
    path.extension().and_then(|ext| ext.to_str()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> (Vec<String>, Vec<String>) {
        (
            vec!["a".to_string(), "old".to_string(), "c".to_string()],
            vec!["a".to_string(), "new".to_string(), "c".to_string()],
        )
    }

    #[test]
    fn display_rows_maps_every_line_pair_kind() {
        let (base_lines, head_lines) = lines();
        let pairs = vec![
            LinePair::Unchanged { base: 0, head: 0 },
            LinePair::Changed { base: 1, head: 1 },
            LinePair::Added { head: 2 },
            LinePair::Removed { base: 2 },
        ];
        let rows = display_rows(&pairs, &base_lines, &head_lines);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, RowKind::Unchanged);
        assert_eq!(rows[0].base, Some(0));
        assert_eq!(rows[0].head, Some(0));

        assert_eq!(rows[1].kind, RowKind::Changed);
        assert!(
            !rows[1].base_spans.is_empty(),
            "changed pair has intraline spans"
        );
        assert!(!rows[1].head_spans.is_empty());

        assert_eq!(rows[2].kind, RowKind::Added);
        assert_eq!(rows[2].base, None);
        assert_eq!(rows[2].head, Some(2));

        assert_eq!(rows[3].kind, RowKind::Removed);
        assert_eq!(rows[3].base, Some(2));
        assert_eq!(rows[3].head, None);
    }

    #[test]
    fn unified_rows_splits_changed_into_removed_then_added() {
        let (base_lines, head_lines) = lines();
        let pairs = vec![LinePair::Changed { base: 1, head: 1 }];
        let rows = unified_rows(&pairs, &base_lines, &head_lines);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, UnifiedKind::Removed);
        assert_eq!(rows[0].base, Some(1));
        assert!(!rows[0].spans.is_empty());
        assert_eq!(rows[1].kind, UnifiedKind::Added);
        assert_eq!(rows[1].head, Some(1));
        assert!(!rows[1].spans.is_empty());
    }

    #[test]
    fn unified_rows_maps_unchanged_added_removed_1_to_1() {
        let (base_lines, head_lines) = lines();
        let pairs = vec![
            LinePair::Unchanged { base: 0, head: 0 },
            LinePair::Added { head: 2 },
            LinePair::Removed { base: 2 },
        ];
        let rows = unified_rows(&pairs, &base_lines, &head_lines);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, UnifiedKind::Unchanged);
        assert_eq!(rows[1].kind, UnifiedKind::Added);
        assert_eq!(rows[2].kind, UnifiedKind::Removed);
    }

    #[test]
    fn language_for_uses_file_extension() {
        assert_eq!(language_for(Path::new("src/main.rs")), "rs");
        assert_eq!(language_for(Path::new("lib/foo.ex")), "ex");
        assert_eq!(language_for(Path::new("README")), "");
    }
}
