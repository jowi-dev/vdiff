//! Embedded-nvim pane: renders the shared [`GridState`] a live
//! [`NvimSession`] publishes into, and translates raw egui input events
//! into nvim key notation for [`NvimCmd::Input`]. `Ctrl-w h`/`Ctrl-w l`
//! pane-switch interception is *not* here -- that's a glue-level concern
//! in [`crate::ui::eframe_app`], which already owns the pending-chord
//! state the built-in viewer's `Ctrl-w` binding uses. Everything in this
//! module either paints (impure, uses `egui::Ui`/`Painter`) or is a pure
//! function unit-tested below (`cols_rows_for_size`, `colors_for`,
//! `translate_event_for_nvim`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use egui::{Color32, Context, Event, FontId, Key, Rect, Ui, Vec2};

use crate::nvim::grid::GridState;
use crate::nvim::session::{NvimCmd, NvimSession};

/// The monospace font size the grid is painted at. Fixed for this spike --
/// no font-size settings/zoom.
const FONT_SIZE: f32 = 14.0;

/// Owns a live [`NvimSession`] plus the cols/rows last sent to it, so
/// [`show`] only fires [`NvimCmd::Resize`] when the pane's pixel size
/// actually changes the cell grid (debounced -- a resizing SidePanel
/// repaints every frame, but the grid only needs to hear about it once per
/// distinct cols/rows pair).
pub struct NvimPane {
    session: NvimSession,
    cols: u16,
    rows: u16,
}

impl NvimPane {
    /// Spawn `nvim --embed` in `cwd`, sized to `cols`x`rows`. `ctx` is
    /// cloned into the session's reader thread so it can request a
    /// repaint after every flushed redraw batch.
    pub fn spawn(cwd: &Path, cols: u16, rows: u16, ctx: Context) -> io::Result<Self> {
        let session = NvimSession::spawn(cwd, cols, rows, move || ctx.request_repaint())?;
        Ok(NvimPane {
            session,
            cols,
            rows,
        })
    }

    /// Forward a raw command to the session (input, explicit resize, open
    /// file).
    pub fn send(&self, cmd: NvimCmd) {
        self.session.send(cmd);
    }

    /// Open `path` at `line` (1-based), following graph focus.
    pub fn open_file(&self, path: PathBuf, line: Option<u64>) {
        self.session.send(NvimCmd::OpenFile(path, line));
    }

    /// Resize the nvim UI to `new_cols`x`new_rows` if that differs from the
    /// last size sent -- see the struct doc for why this is debounced here
    /// rather than firing on every frame.
    pub fn maybe_resize(&mut self, new_cols: u16, new_rows: u16) {
        if new_cols == 0 || new_rows == 0 {
            return;
        }
        if new_cols != self.cols || new_rows != self.rows {
            self.cols = new_cols;
            self.rows = new_rows;
            self.session.send(NvimCmd::Resize(new_cols, new_rows));
        }
    }

    /// The shared grid, for [`show`] to lock and paint from.
    pub fn grid(&self) -> Arc<Mutex<GridState>> {
        self.session.grid()
    }

    /// Whether the underlying session still believes `nvim` is alive (see
    /// [`NvimSession::is_alive`]). `false` after e.g. `ZZ`/`:q` exits the
    /// process -- the eframe glue uses this to bounce focus back to the
    /// graph pane and to decide whether [`Self::open_file`] needs a
    /// respawn first.
    pub fn is_alive(&self) -> bool {
        self.session.is_alive()
    }

    /// The cols/rows last sent to nvim -- read by the eframe glue when
    /// respawning, so the replacement session starts at the same size
    /// rather than snapping back to a default and immediately resizing.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}

/// The message shown in place of the grid once nvim has exited (`ZZ`, `:q`,
/// a crash, ...) and the glue hasn't respawned it yet. `eframe_app` moves
/// keyboard focus off this pane the moment it notices [`NvimPane::is_alive`]
/// is `false` (see [`crate::ui::eframe_app::VdiffApp::logic`]), so this text
/// is only ever visible for at most one frame in practice -- but it's the
/// fallback if that race ever loses, rather than silently painting a
/// stale, frozen-looking grid.
const DEAD_MESSAGE: &str = "nvim exited — move focus or press Enter to relaunch";

/// Paint `pane`'s current grid into the remaining space of `ui`, resizing
/// the nvim UI first if the available space implies a different cols/rows
/// than last frame. `focused` draws the block cursor -- matches the
/// built-in file viewer's `focused`-gated border/cursor convention. If the
/// session has died (see [`NvimPane::is_alive`]), paints [`DEAD_MESSAGE`]
/// instead of the (stale) grid.
pub fn show(ui: &mut Ui, pane: &mut NvimPane, focused: bool) {
    if !pane.is_alive() {
        ui.centered_and_justified(|ui| ui.label(DEAD_MESSAGE));
        return;
    }

    let font_id = FontId::monospace(FONT_SIZE);
    let ctx = ui.ctx().clone();
    let row_height = ctx.fonts_mut(|f| f.row_height(&font_id));
    let char_width = ctx.fonts_mut(|f| f.glyph_width(&font_id, 'M'));

    let avail = ui.available_size();
    let (cols, rows) = cols_rows_for_size(avail.x, avail.y, char_width, row_height);
    pane.maybe_resize(cols, rows);

    let origin = ui.max_rect().min;
    let grid_arc = pane.grid();
    let Ok(grid) = grid_arc.lock() else { return };

    let painter = ui.painter();
    for row in 0..grid.rows {
        paint_row(
            painter, &grid, row, origin, char_width, row_height, &font_id,
        );
    }

    if focused {
        let (cursor_row, cursor_col) = grid.cursor;
        let rect = Rect::from_min_size(
            origin
                + Vec2::new(
                    cursor_col as f32 * char_width,
                    cursor_row as f32 * row_height,
                ),
            Vec2::new(char_width, row_height),
        );
        painter.rect_filled(rect, 0.0, Color32::from_white_alpha(110));
    }

    // Reserve the space so sibling widgets/the panel layout account for it.
    ui.allocate_space(Vec2::new(
        grid.cols as f32 * char_width,
        grid.rows as f32 * row_height,
    ));
}

/// Paint one row, batching consecutive same-highlight cells into a single
/// background rect + text run rather than one draw call per cell.
fn paint_row(
    painter: &egui::Painter,
    grid: &GridState,
    row: usize,
    origin: egui::Pos2,
    char_width: f32,
    row_height: f32,
    font_id: &FontId,
) {
    let mut col = 0;
    while col < grid.cols {
        let Some(start_cell) = grid.cell(row, col) else {
            break;
        };
        let hl_id = start_cell.hl_id;
        let run_start = col;
        let mut text = String::new();
        while col < grid.cols {
            match grid.cell(row, col) {
                Some(cell) if cell.hl_id == hl_id => {
                    text.push_str(&cell.ch);
                    col += 1;
                }
                _ => break,
            }
        }
        let (fg, bg) = colors_for(grid, hl_id);
        let rect = Rect::from_min_size(
            origin + Vec2::new(run_start as f32 * char_width, row as f32 * row_height),
            Vec2::new((col - run_start) as f32 * char_width, row_height),
        );
        painter.rect_filled(rect, 0.0, bg);
        painter.text(rect.min, egui::Align2::LEFT_TOP, &text, font_id.clone(), fg);
    }
}

/// The (fg, bg) [`Color32`] pair for `hl_id`: `grid`'s defaults if `hl_id`
/// has no entry (or is `0`, nvim's "default" group), with `reverse`
/// swapping fg/bg. Bold/italic are not applied to the paint job -- see the
/// module's report note on why (egui's default monospace font has no bold/
/// italic face wired up in this app, and faking it with a second font
/// family was out of scope for the spike).
fn colors_for(grid: &GridState, hl_id: u64) -> (Color32, Color32) {
    let default_fg = u32_to_color(grid.default_fg);
    let default_bg = u32_to_color(grid.default_bg);
    let Some(attr) = grid.hl_attrs.get(&hl_id) else {
        return (default_fg, default_bg);
    };
    let mut fg = attr.fg.map(u32_to_color).unwrap_or(default_fg);
    let mut bg = attr.bg.map(u32_to_color).unwrap_or(default_bg);
    if attr.reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

/// A 24-bit `0xRRGGBB` int (as nvim sends colors) to [`Color32`].
fn u32_to_color(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// How many whole cell columns/rows fit in `width`x`height` pixels of
/// `char_width`x`row_height` cells. Clamped to at least 1x1 so a
/// too-small panel never asks nvim to resize to 0.
pub fn cols_rows_for_size(width: f32, height: f32, char_width: f32, row_height: f32) -> (u16, u16) {
    if char_width <= 0.0 || row_height <= 0.0 {
        return (1, 1);
    }
    let cols = (width / char_width).floor().max(1.0) as u16;
    let rows = (height / row_height).floor().max(1.0) as u16;
    (cols, rows)
}

/// Translate one egui input `event` into the string to send via
/// [`NvimCmd::Input`], or `None` if this event carries nothing nvim should
/// see.
///
/// - [`Event::Text`] is forwarded as-is, except `<` (which nvim's key
///   notation would otherwise parse as the start of a `<...>` token) is
///   escaped to `<lt>`.
/// - [`Event::Key`] is *only* translated for keys with no companion
///   `Text` event: navigation/editing specials (`Esc`, `Enter`,
///   `Backspace`, `Tab`, arrows, `Delete`, `Home`/`End`, `PageUp`/`Down`)
///   in angle notation, and Ctrl-held letters as `<C-x>`. Plain printable
///   keys return `None` here -- egui emits both a `Key` press and a
///   `Text` event for them, and forwarding both would double-type. `Esc`
///   is intentionally included (not intercepted) -- nvim is modal, and a
///   local Esc-closes-the-pane binding would break leaving insert mode.
pub fn translate_event_for_nvim(event: &Event) -> Option<String> {
    match event {
        Event::Text(text) => {
            if text.is_empty() {
                None
            } else {
                Some(text.replace('<', "<lt>"))
            }
        }
        Event::Key {
            key,
            pressed,
            modifiers,
            ..
        } if *pressed => {
            if let Some(special) = special_key_notation(*key) {
                return Some(special.to_string());
            }
            if modifiers.ctrl {
                if let Some(c) = letter_char(*key) {
                    return Some(format!("<C-{c}>"));
                }
            }
            None
        }
        _ => None,
    }
}

/// Angle-notation for keys that have no companion [`Event::Text`].
fn special_key_notation(key: Key) -> Option<&'static str> {
    match key {
        Key::Escape => Some("<Esc>"),
        Key::Enter => Some("<CR>"),
        Key::Backspace => Some("<BS>"),
        Key::Tab => Some("<Tab>"),
        Key::ArrowUp => Some("<Up>"),
        Key::ArrowDown => Some("<Down>"),
        Key::ArrowLeft => Some("<Left>"),
        Key::ArrowRight => Some("<Right>"),
        Key::Delete => Some("<Del>"),
        Key::Home => Some("<Home>"),
        Key::End => Some("<End>"),
        Key::PageUp => Some("<PageUp>"),
        Key::PageDown => Some("<PageDown>"),
        _ => None,
    }
}

/// The lowercase ascii letter a single-letter key represents (`Key::A` ->
/// `'a'`), or `None` for any other key -- used only for `<C-x>` notation,
/// where `x` must be the bare letter.
fn letter_char(key: Key) -> Option<char> {
    let name = key.name();
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_none() && c.is_ascii_alphabetic() {
        Some(c.to_ascii_lowercase())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvim::grid::{Cell, HlAttr};
    use egui::Modifiers;

    #[test]
    fn cols_rows_for_size_floors_and_clamps_to_one() {
        assert_eq!(cols_rows_for_size(100.0, 40.0, 10.0, 20.0), (10, 2));
        assert_eq!(cols_rows_for_size(5.0, 5.0, 10.0, 20.0), (1, 1));
        assert_eq!(cols_rows_for_size(100.0, 100.0, 0.0, 20.0), (1, 1));
    }

    #[test]
    fn colors_for_falls_back_to_defaults_with_no_hl_entry() {
        let grid = GridState::new(1, 1);
        let (fg, bg) = colors_for(&grid, 42);
        assert_eq!(fg, u32_to_color(grid.default_fg));
        assert_eq!(bg, u32_to_color(grid.default_bg));
    }

    #[test]
    fn colors_for_uses_hl_attr_fg_bg() {
        let mut grid = GridState::new(1, 1);
        grid.hl_attrs.insert(
            1,
            HlAttr {
                fg: Some(0x00_ff_00),
                bg: Some(0x00_00_ff),
                ..Default::default()
            },
        );
        assert_eq!(
            colors_for(&grid, 1),
            (u32_to_color(0x00_ff_00), u32_to_color(0x00_00_ff))
        );
    }

    #[test]
    fn colors_for_reverse_swaps_fg_and_bg() {
        let mut grid = GridState::new(1, 1);
        grid.hl_attrs.insert(
            1,
            HlAttr {
                fg: Some(0x00_ff_00),
                bg: Some(0x00_00_ff),
                reverse: true,
                ..Default::default()
            },
        );
        assert_eq!(
            colors_for(&grid, 1),
            (u32_to_color(0x00_00_ff), u32_to_color(0x00_ff_00))
        );
    }

    #[test]
    fn u32_to_color_splits_channels() {
        assert_eq!(
            u32_to_color(0x11_22_33),
            Color32::from_rgb(0x11, 0x22, 0x33)
        );
    }

    fn key_event(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn text_event_forwards_as_is() {
        assert_eq!(
            translate_event_for_nvim(&Event::Text("x".to_string())),
            Some("x".to_string())
        );
    }

    #[test]
    fn text_event_empty_string_is_none() {
        assert_eq!(translate_event_for_nvim(&Event::Text(String::new())), None);
    }

    #[test]
    fn text_event_escapes_less_than() {
        assert_eq!(
            translate_event_for_nvim(&Event::Text("<".to_string())),
            Some("<lt>".to_string())
        );
        assert_eq!(
            translate_event_for_nvim(&Event::Text("a<b".to_string())),
            Some("a<lt>b".to_string())
        );
    }

    #[test]
    fn special_keys_translate_to_angle_notation() {
        let cases = [
            (Key::Escape, "<Esc>"),
            (Key::Enter, "<CR>"),
            (Key::Backspace, "<BS>"),
            (Key::Tab, "<Tab>"),
            (Key::ArrowUp, "<Up>"),
            (Key::ArrowDown, "<Down>"),
            (Key::ArrowLeft, "<Left>"),
            (Key::ArrowRight, "<Right>"),
            (Key::Delete, "<Del>"),
            (Key::Home, "<Home>"),
            (Key::End, "<End>"),
            (Key::PageUp, "<PageUp>"),
            (Key::PageDown, "<PageDown>"),
        ];
        for (key, expected) in cases {
            assert_eq!(
                translate_event_for_nvim(&key_event(key, Modifiers::NONE)),
                Some(expected.to_string()),
                "key={key:?}"
            );
        }
    }

    #[test]
    fn plain_printable_key_event_returns_none_favoring_text_event() {
        // Letters/digits arrive as both a Key press and a Text event; the
        // Key half must be dropped so input isn't doubled.
        assert_eq!(
            translate_event_for_nvim(&key_event(Key::A, Modifiers::NONE)),
            None
        );
        assert_eq!(
            translate_event_for_nvim(&key_event(Key::Num1, Modifiers::NONE)),
            None
        );
    }

    #[test]
    fn ctrl_letter_translates_to_c_notation() {
        assert_eq!(
            translate_event_for_nvim(&key_event(Key::W, Modifiers::CTRL)),
            Some("<C-w>".to_string())
        );
        assert_eq!(
            translate_event_for_nvim(&key_event(Key::R, Modifiers::CTRL)),
            Some("<C-r>".to_string())
        );
    }

    #[test]
    fn unreleased_key_event_is_none() {
        let mut event = key_event(Key::Escape, Modifiers::NONE);
        if let Event::Key { pressed, .. } = &mut event {
            *pressed = false;
        }
        assert_eq!(translate_event_for_nvim(&event), None);
    }

    #[test]
    fn unrelated_events_are_none() {
        assert_eq!(translate_event_for_nvim(&Event::Copy), None);
    }

    #[test]
    fn cell_default_is_blank_space_hl_zero() {
        assert_eq!(
            Cell::default(),
            Cell {
                ch: " ".to_string(),
                hl_id: 0
            }
        );
    }
}
