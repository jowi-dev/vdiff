//! Pure [`crate::nvim::grid::GridState`] -> `ratatui::buffer::Buffer`
//! paint, the terminal twin of the GUI's egui-based grid paint in
//! `crate::ui::nvim_pane`. Grid coordinates map 1:1 onto the target
//! `Rect` starting at its top-left corner; anything past either the
//! grid's or the area's extent (whichever is smaller) is simply not
//! drawn -- there's no scrolling/offset concept here, that's the caller's
//! job when it picks the `Rect` to render into.
//!
//! `hl_id` -> color/attribute resolution intentionally treats "no
//! `HlAttr` entry for this id" the same as "an `HlAttr` entry with `fg`/
//! `bg` left `None`" -- both fall back to [`GridState::default_fg`]/
//! [`GridState::default_bg`], since hl id `0` (nvim's "default
//! highlight") never gets an explicit `hl_attr_define` of its own.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::nvim::grid::GridState;

/// Paint `grid` into `buf` starting at `area`'s top-left corner, clipped to
/// `min(grid dims, area dims)`. When `draw_cursor` is true, the cell under
/// `grid.cursor` additionally gets [`Modifier::REVERSED`] on top of its own
/// highlight styling (bounds-checked against the same clipped extent, so a
/// cursor position nvim hasn't caught up to resizing yet is silently a
/// no-op rather than a panic).
pub fn render_grid(grid: &GridState, area: Rect, buf: &mut Buffer, draw_cursor: bool) {
    let cols = grid.cols.min(area.width as usize);
    let rows = grid.rows.min(area.height as usize);
    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = grid.cell(row, col) else {
                continue;
            };
            // ext_linegrid emits a double-width glyph followed by an
            // empty-string continuation cell for the column it visually
            // occupies but doesn't own text in -- leave the buffer cell
            // exactly as the wide glyph's own write left it.
            if cell.ch.is_empty() {
                continue;
            }
            let x = area.x + col as u16;
            let y = area.y + row as u16;
            let style = style_for(grid, cell.hl_id);
            let buf_cell = &mut buf[(x, y)];
            buf_cell.set_symbol(&cell.ch);
            buf_cell.set_style(style);
        }
    }
    if draw_cursor {
        let (crow, ccol) = grid.cursor;
        if crow < rows && ccol < cols {
            let x = area.x + ccol as u16;
            let y = area.y + crow as u16;
            buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
        }
    }
}

/// The `ratatui::style::Style` for one cell's `hl_id`: colors from the
/// matching [`HlAttr`] (falling back to `grid`'s default fg/bg when the
/// attr has no entry, or has one but leaves that field `None`), plus
/// bold/italic/underline/reverse mapped straight onto the matching
/// `ratatui` [`Modifier`] bit.
fn style_for(grid: &GridState, hl_id: u64) -> Style {
    let attr = grid.hl_attrs.get(&hl_id).copied().unwrap_or_default();
    let fg = attr.fg.unwrap_or(grid.default_fg);
    let bg = attr.bg.unwrap_or(grid.default_bg);
    let mut style = Style::default().fg(rgb_color(fg)).bg(rgb_color(bg));
    if attr.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if attr.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if attr.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if attr.reverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// A packed `0xRRGGBB` value (as nvim's `default_colors_set`/
/// `hl_attr_define` send them) to a `ratatui::style::Color::Rgb`.
fn rgb_color(rgb: u32) -> Color {
    Color::Rgb(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::nvim::grid::{Cell, HlAttr, RedrawEvent};

    /// A blank `cols`x`rows` grid with `fg`/`bg` set as its defaults --
    /// every test starts from this rather than `GridState::new`'s raw
    /// white-on-black so default-color-fallback assertions have a
    /// distinctive, unmistakable value to check for.
    fn grid(cols: usize, rows: usize) -> GridState {
        let mut grid = GridState::new(cols, rows);
        grid.apply(&RedrawEvent::DefaultColorsSet {
            fg: 0x11_22_33,
            bg: 0x44_55_66,
        });
        grid
    }

    fn write_line(grid: &mut GridState, row: usize, col_start: usize, cells: Vec<Cell>) {
        grid.apply(&RedrawEvent::GridLine {
            row,
            col_start,
            cells,
        });
    }

    fn cell(ch: &str, hl_id: u64) -> Cell {
        Cell {
            ch: ch.to_string(),
            hl_id,
        }
    }

    fn define(grid: &mut GridState, id: u64, attr: HlAttr) {
        grid.apply(&RedrawEvent::HlAttrDefine { id, attr });
    }

    #[test]
    fn renders_basic_text() {
        let mut g = grid(5, 1);
        write_line(&mut g, 0, 0, vec![cell("h", 0), cell("i", 0), cell("!", 0)]);
        let area = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(1, 0)].symbol(), "i");
        assert_eq!(buf[(2, 0)].symbol(), "!");
        // untouched columns keep the buffer's own blank default
        assert_eq!(buf[(3, 0)].symbol(), " ");
    }

    #[test]
    fn maps_hl_attr_colors() {
        let mut g = grid(2, 1);
        define(
            &mut g,
            7,
            HlAttr {
                fg: Some(0xff_00_00),
                bg: Some(0x00_ff_00),
                ..Default::default()
            },
        );
        write_line(&mut g, 0, 0, vec![cell("x", 7)]);
        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(0xff, 0x00, 0x00));
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0x00, 0xff, 0x00));
    }

    #[test]
    fn falls_back_to_default_colors_when_hl_id_unknown() {
        let mut g = grid(1, 1);
        write_line(&mut g, 0, 0, vec![cell("x", 99)]);
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(0x11, 0x22, 0x33));
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0x44, 0x55, 0x66));
    }

    #[test]
    fn falls_back_to_default_colors_when_attr_leaves_fields_none() {
        let mut g = grid(1, 1);
        define(&mut g, 3, HlAttr::default());
        write_line(&mut g, 0, 0, vec![cell("x", 3)]);
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(0x11, 0x22, 0x33));
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(0x44, 0x55, 0x66));
    }

    #[test]
    fn reverse_attr_sets_reversed_modifier() {
        let mut g = grid(1, 1);
        define(
            &mut g,
            4,
            HlAttr {
                reverse: true,
                ..Default::default()
            },
        );
        write_line(&mut g, 0, 0, vec![cell("x", 4)]);
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert!(buf[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn bold_and_italic_attrs_set_modifiers() {
        let mut g = grid(1, 1);
        define(
            &mut g,
            5,
            HlAttr {
                bold: true,
                italic: true,
                ..Default::default()
            },
        );
        write_line(&mut g, 0, 0, vec![cell("x", 5)]);
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert!(buf[(0, 0)].modifier.contains(Modifier::ITALIC));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn underline_attr_sets_modifier() {
        let mut g = grid(1, 1);
        define(
            &mut g,
            6,
            HlAttr {
                underline: true,
                ..Default::default()
            },
        );
        write_line(&mut g, 0, 0, vec![cell("x", 6)]);
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert!(buf[(0, 0)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn clips_to_area_smaller_than_grid() {
        let mut g = grid(5, 5);
        for row in 0..5 {
            write_line(&mut g, row, 0, (0..5).map(|_| cell("x", 0)).collect());
        }
        let area = Rect::new(0, 0, 2, 2);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "x");
        assert_eq!(buf[(1, 1)].symbol(), "x");
        // area is only 2x2, so nothing past that was ever touched -- just
        // confirm the render didn't panic walking off either edge.
    }

    #[test]
    fn clips_to_grid_smaller_than_area() {
        let mut g = grid(2, 1);
        write_line(&mut g, 0, 0, vec![cell("a", 0), cell("b", 0)]);
        let area = Rect::new(0, 0, 5, 3);
        let mut buf = Buffer::empty(area);
        render_grid(&g, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(1, 0)].symbol(), "b");
        // rows/cols beyond the grid's own extent are left as the buffer's
        // own blank default, never indexed into the (smaller) grid.
        assert_eq!(buf[(2, 0)].symbol(), " ");
        assert_eq!(buf[(0, 1)].symbol(), " ");
    }

    #[test]
    fn empty_string_continuation_cell_does_not_overwrite_wide_glyph() {
        let mut g = grid(2, 1);
        write_line(&mut g, 0, 0, vec![cell("\u{6c49}", 0), cell("", 0)]);
        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        // pre-seed the continuation column the way a real wide-glyph write
        // via `Buffer::set_string` would leave it, so we can prove
        // `render_grid` left it alone rather than stomping it with a blank.
        buf[(1, 0)].set_symbol("");
        render_grid(&g, area, &mut buf, false);
        assert_eq!(buf[(0, 0)].symbol(), "\u{6c49}");
        assert_eq!(buf[(1, 0)].symbol(), "");
    }

    #[test]
    fn cursor_cell_is_reversed_only_when_draw_cursor_true() {
        let mut g = grid(2, 1);
        write_line(&mut g, 0, 0, vec![cell("a", 0), cell("b", 0)]);
        g.apply(&RedrawEvent::GridCursorGoto { row: 0, col: 1 });
        let area = Rect::new(0, 0, 2, 1);

        let mut buf_no_cursor = Buffer::empty(area);
        render_grid(&g, area, &mut buf_no_cursor, false);
        assert!(!buf_no_cursor[(1, 0)].modifier.contains(Modifier::REVERSED));

        let mut buf_cursor = Buffer::empty(area);
        render_grid(&g, area, &mut buf_cursor, true);
        assert!(buf_cursor[(1, 0)].modifier.contains(Modifier::REVERSED));
        // the non-cursor cell is untouched by the cursor pass
        assert!(!buf_cursor[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn cursor_out_of_clipped_bounds_is_a_no_op() {
        let mut g = grid(5, 5);
        g.apply(&RedrawEvent::GridCursorGoto { row: 4, col: 4 });
        let area = Rect::new(0, 0, 2, 2);
        let mut buf = Buffer::empty(area);
        // must not panic despite the cursor sitting outside the clipped
        // 2x2 area this call actually renders.
        render_grid(&g, area, &mut buf, true);
    }
}
