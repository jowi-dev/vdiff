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
use std::time::Duration;

use egui::{Color32, Context, Event, FontId, Key, Modifiers, Rect, Ui, Vec2};
use rmpv::Value;

use crate::nvim::grid::GridState;
use crate::nvim::session::{NvimCmd, NvimSession};

/// The monospace font size the grid is painted at. Fixed for this spike --
/// no font-size settings/zoom.
const FONT_SIZE: f32 = 14.0;

/// How long a boundary-detection [`NvimPane::at_boundary`] call (or any
/// other [`NvimPane::call`]) blocks the calling (UI) thread waiting for
/// nvim's response before giving up and treating it as a boundary/failure.
/// Short enough that a wedged-but-not-dead nvim can never make `Ctrl-w`
/// navigation feel stuck.
const CALL_TIMEOUT: Duration = Duration::from_millis(100);

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

    /// Open `path` at `line` (1-based), following graph focus, marking
    /// every changed-head-line range in `ranges` (0-based, inclusive --
    /// see `pipeline::file_diff::changed_head_ranges`) with a highlight
    /// and gutter sign. `ranges` empty is correct (and clears any marks
    /// left over from whatever was in this buffer slot before) for an
    /// unchanged node or a deleted one.
    pub fn open_file(&self, path: PathBuf, line: Option<u64>, ranges: Vec<(usize, usize)>) {
        self.session.send(NvimCmd::OpenFile { path, line, ranges });
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

    /// Call `method(params)` and wait up to [`CALL_TIMEOUT`] for the
    /// response -- see [`NvimSession::call`] for the exact semantics
    /// (`None` covers RPC error, timeout, *and* a dead session uniformly).
    pub fn call(&self, method: &str, params: Vec<Value>) -> Option<Value> {
        self.session.call(method, params, CALL_TIMEOUT)
    }

    /// Whether the current window is already at nvim's split boundary in
    /// direction `dir` (`"h"` or `"l"`) -- i.e. `winnr()` and `winnr(dir)`
    /// agree there's nowhere further to move. Used to decide whether a
    /// `Ctrl-w h`/`Ctrl-w l` (or the arrow-key aliases) should hop out to
    /// vdiff's graph pane (at the boundary) or forward into nvim's own
    /// split navigation (not at the boundary, nvim has internal splits to
    /// move between). On *any* failure to get a straight answer --
    /// timeout, RPC error, dead session -- conservatively reports `true`
    /// ("at boundary"): a wedged-but-not-dead nvim must never be able to
    /// trap keyboard focus on this pane, so when in doubt, let the user
    /// out.
    pub fn at_boundary(&self, dir: &str) -> bool {
        let winnr = |args: Vec<Value>| {
            self.call(
                "nvim_call_function",
                vec![Value::from("winnr"), Value::Array(args)],
            )
        };
        match (winnr(vec![]), winnr(vec![Value::from(dir)])) {
            (Some(here), Some(there)) => here == there,
            _ => true,
        }
    }

    /// Run one Ex command via [`Self::call`] (rather than the
    /// fire-and-forget [`NvimCmd::Ex`]) so a genuine failure can be logged
    /// -- used for `--nvim-cmd` init commands, both on first spawn and
    /// every respawn. Returns `Ok(())` if nvim reported no error, `Err`
    /// with a message worth printing as a warning otherwise (including a
    /// timeout/dead session, which can't be told apart from a real nvim
    /// error at this layer -- see [`NvimSession::call`]).
    pub fn run_init_command(&self, command: &str) -> Result<(), String> {
        match self.call("nvim_command", vec![Value::from(command)]) {
            Some(_) => Ok(()),
            None => Err(format!(
                "nvim-cmd '{command}' failed or timed out (no response within {CALL_TIMEOUT:?})"
            )),
        }
    }

    /// Register `:VdiffDiff`/`:VdiffDiffOff` in this session -- fresh
    /// children (initial spawn, and every respawn) start with no user
    /// commands at all, so this has to run every time a session comes up,
    /// not just once at startup. Fire-and-forget like [`NvimCmd::Ex`]
    /// generally -- a failure here (should never happen; these are static,
    /// always-valid command strings) just means `:VdiffDiff` won't exist
    /// this session, not a crash.
    pub fn register_vdiff_commands(&self) {
        self.send(NvimCmd::Ex(
            crate::nvim::session::VDIFF_DIFF_COMMAND.to_string(),
        ));
        self.send(NvimCmd::Ex(
            crate::nvim::session::VDIFF_DIFF_OFF_COMMAND.to_string(),
        ));
    }

    /// Whether `:VdiffDiff` was invoked in this session since the last
    /// call -- see [`NvimSession::take_diff_request`].
    pub fn take_diff_request(&self) -> bool {
        self.session.take_diff_request()
    }

    /// Open (or refresh) the diffsplit-against-merge-base view for
    /// whatever file is currently open: a read-only scratch buffer holding
    /// `base_content`, vertically split to the left, both windows in
    /// `diffthis` mode. `path` names the scratch buffer (`vdiff-base://
    /// <path>`) and picks its filetype for syntax highlighting -- see
    /// [`NvimCmd::DiffSplit`].
    pub fn diffsplit(&self, path: PathBuf, base_content: String) {
        self.send(NvimCmd::DiffSplit { path, base_content });
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
                if let Some(c) = single_char(*key) {
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

/// The lowercase ascii letter/digit a single-character key represents
/// (`Key::A` -> `'a'`, `Key::Num1` -> `'1'`), or `None` for any other key --
/// used for `<C-x>` notation (where `x` must be the bare letter) and, via
/// [`ctrl_w_continuation`], for forwarding an arbitrary un-intercepted
/// `Ctrl-w <key>` continuation to nvim.
fn single_char(key: Key) -> Option<char> {
    let name = key.name();
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_none() && c.is_ascii_alphanumeric() {
        Some(c.to_ascii_lowercase())
    } else {
        None
    }
}

/// Build the nvim key-notation to forward for a completed `Ctrl-w` chord
/// whose second key isn't one of the boundary-aware bindings
/// (`h`/`l`/arrows -- handled separately in
/// [`crate::ui::eframe_app::VdiffApp::handle_nvim_keys`], since they need
/// [`NvimPane::at_boundary`], not a pure translation). Restores the rest of
/// nvim's `Ctrl-w` repertoire (`Ctrl-w q` close, `Ctrl-w o` only-this-window,
/// `Ctrl-w w`/`Ctrl-w Ctrl-w` cycle, `Ctrl-w j`/`k` move down/up, ...) that
/// a blanket "clear the chord silently" would otherwise have swallowed.
///
/// - `j`/`k`/`ArrowUp`/`ArrowDown` and their `Ctrl-w Ctrl-j`-style variants
///   *are* included here (always forwarded, never boundary-checked) --
///   vertical splits are entirely nvim-internal, vdiff has no pane above or
///   below to hop to.
/// - A second key held with Ctrl becomes a nested `<C-x>` (`Ctrl-w Ctrl-w`
///   -> `<C-w><C-w>`), matching nvim's own idiom for that chord.
/// - A special key (no ctrl) uses its angle notation as-is (`<C-w><Esc>`).
/// - Anything else falls back to its bare letter/digit.
///
/// Returns `None` only if `key` maps to nothing at all (a key
/// [`special_key_notation`] and [`single_char`] both reject -- function
/// keys, media keys, etc. -- vanishingly unlikely to reach here but kept
/// total rather than panicking).
pub fn ctrl_w_continuation(key: Key, modifiers: Modifiers) -> Option<String> {
    if modifiers.ctrl {
        if let Some(c) = single_char(key) {
            return Some(format!("<C-w><C-{c}>"));
        }
    }
    if let Some(special) = special_key_notation(key) {
        return Some(format!("<C-w>{special}"));
    }
    let c = single_char(key)?;
    Some(format!("<C-w>{c}"))
}

/// One decision produced by [`process_nvim_events`] for the eframe glue to
/// execute. Boundary-checking isn't resolved here -- it needs a live
/// `at_boundary` RPC round-trip, which is impure -- so
/// [`CtrlWBoundary`](NvimAction::CtrlWBoundary) just carries what the
/// caller needs to resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvimAction {
    /// Send this nvim key notation via `nvim_input`, forwarded as-is.
    Input(String),
    /// A completed `Ctrl-w h`/`Ctrl-w l`/`Ctrl-w Left`/`Ctrl-w Right`.
    /// Caller: query `NvimPane::at_boundary(dir)`; at the boundary,
    /// dispatch `Msg::PaneLeft` if `hop_left` (a no-op at the right
    /// boundary -- there's no pane further right than the nvim pane);
    /// otherwise send `forward_seq` so nvim moves between its own splits.
    CtrlWBoundary {
        dir: &'static str,
        hop_left: bool,
        forward_seq: &'static str,
    },
}

/// Process one frame's raw egui input events for the nvim pane, given
/// whether a `Ctrl-w` chord was left armed from the previous frame (see
/// [`crate::ui::eframe_app::VdiffApp::handle_nvim_keys`]). Returns the
/// actions to execute, in order, and the chord-armed state to carry into
/// the next frame's call.
///
/// This is the one place that has to get egui's `Key`/`Text` event pairing
/// right for the `Ctrl-w` chord specifically (ordinary forwarding's
/// dedup -- drop `Key` for printables, forward `Text` -- doesn't care about
/// event order at all, since a plain printable `Key` press already
/// translates to `None` regardless of state; see
/// [`translate_event_for_nvim`]). Two failure modes fixed here:
///
/// 1. egui delivers a *release* `Key` event too (`pressed: false`) for
///    every press, including `Ctrl-w` itself. Treating "any `Key` event
///    while armed" as the completing keystroke means the `Ctrl-w`
///    *release* disarms the chord before the user's next real keypress
///    ever arrives -- so `h` lands after the chord is already gone and
///    forwards as a plain `h` (cursor-left) instead of completing it.
///    Fixed by only ever treating `pressed: true` `Key` events as
///    significant while armed; releases (and anything else) are ignored
///    without disarming.
/// 2. Once the chord *is* completed by the `h` `Key` press, egui's paired
///    `Text("h")` event still shows up right after -- and ordinary
///    forwarding would send it verbatim, double-firing the keystroke (once
///    as the chord's resolution, once as a leaked literal `h`). Fixed by
///    suppressing exactly one following `Text` event after a chord
///    consumes a `Key`.
pub fn process_nvim_events(events: &[Event], pending: bool) -> (Vec<NvimAction>, bool) {
    let mut actions = Vec::new();
    let mut pending = pending;
    // Set the frame a `Key` event (chord arm or chord completion) was just
    // acted on instead of falling through to ordinary forwarding -- the
    // very next event gets one chance to be that key's paired `Text` event
    // (dropped silently if so) before this reverts to normal handling.
    // Needed regardless of whether egui delivers `Key` before or after
    // `Text` for the same press: if `Text` comes first, the `_ => {}` arm
    // below (while `pending`) already ignores it without disarming, so the
    // `Key` still resolves the chord when it arrives; if `Key` comes first,
    // this flag is what stops the trailing `Text` from also forwarding as
    // a literal keystroke.
    let mut suppress_text = false;
    for event in events {
        if suppress_text {
            suppress_text = false;
            if matches!(event, Event::Text(_)) {
                continue;
            }
            // Not the paired Text after all -- handle this event normally below.
        }

        if pending {
            match event {
                // A release (including Ctrl-w's own) is not a completing
                // keystroke -- ignore it without disarming, so the chord
                // survives until a real *press* arrives.
                Event::Key { pressed: false, .. } => {}
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    pending = false;
                    suppress_text = true;
                    push_chord_completion(&mut actions, *key, *modifiers);
                }
                // No legitimate way to complete the chord via a bare
                // `Text` event (every physical press keys also emits a
                // `Key` event) -- ignore anything else while armed.
                _ => {}
            }
            continue;
        }

        if let Event::Key {
            key: Key::W,
            pressed: true,
            modifiers,
            ..
        } = event
        {
            if modifiers.ctrl {
                pending = true;
                suppress_text = true; // defensive: swallow a Text companion if one ever shows up.
                continue;
            }
        }

        if let Some(text) = translate_event_for_nvim(event) {
            actions.push(NvimAction::Input(text));
        }
    }
    (actions, pending)
}

/// The second half of the `Ctrl-w` chord, once a completing `Key` press has
/// been identified: boundary-aware for `h`/`l`/arrow-left/right (see
/// [`NvimAction::CtrlWBoundary`]), always-forward for `j`/`k`/arrow-up/down
/// (vertical splits are entirely nvim-internal -- vdiff has no pane above
/// or below), and `<C-w><key>` via [`ctrl_w_continuation`] for everything
/// else (restores the rest of nvim's `Ctrl-w` repertoire -- `q`, `o`, `w`,
/// `Ctrl-w`, ... -- rather than silently dropping it).
fn push_chord_completion(actions: &mut Vec<NvimAction>, key: Key, modifiers: Modifiers) {
    match key {
        Key::H => actions.push(NvimAction::CtrlWBoundary {
            dir: "h",
            hop_left: true,
            forward_seq: "<C-w>h",
        }),
        Key::ArrowLeft => actions.push(NvimAction::CtrlWBoundary {
            dir: "h",
            hop_left: true,
            forward_seq: "<C-w><Left>",
        }),
        Key::L => actions.push(NvimAction::CtrlWBoundary {
            dir: "l",
            hop_left: false,
            forward_seq: "<C-w>l",
        }),
        Key::ArrowRight => actions.push(NvimAction::CtrlWBoundary {
            dir: "l",
            hop_left: false,
            forward_seq: "<C-w><Right>",
        }),
        _ => {
            if let Some(seq) = ctrl_w_continuation(key, modifiers) {
                actions.push(NvimAction::Input(seq));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvim::grid::{Cell, HlAttr};

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

    #[test]
    fn ctrl_w_continuation_plain_letter_forwards_bare_char() {
        assert_eq!(
            ctrl_w_continuation(Key::Q, Modifiers::NONE),
            Some("<C-w>q".to_string())
        );
        assert_eq!(
            ctrl_w_continuation(Key::O, Modifiers::NONE),
            Some("<C-w>o".to_string())
        );
        assert_eq!(
            ctrl_w_continuation(Key::W, Modifiers::NONE),
            Some("<C-w>w".to_string())
        );
    }

    #[test]
    fn ctrl_w_continuation_vertical_motion_always_forwards() {
        assert_eq!(
            ctrl_w_continuation(Key::J, Modifiers::NONE),
            Some("<C-w>j".to_string())
        );
        assert_eq!(
            ctrl_w_continuation(Key::K, Modifiers::NONE),
            Some("<C-w>k".to_string())
        );
        assert_eq!(
            ctrl_w_continuation(Key::ArrowUp, Modifiers::NONE),
            Some("<C-w><Up>".to_string())
        );
        assert_eq!(
            ctrl_w_continuation(Key::ArrowDown, Modifiers::NONE),
            Some("<C-w><Down>".to_string())
        );
    }

    #[test]
    fn ctrl_w_continuation_ctrl_letter_nests_c_notation() {
        assert_eq!(
            ctrl_w_continuation(Key::W, Modifiers::CTRL),
            Some("<C-w><C-w>".to_string())
        );
    }

    #[test]
    fn ctrl_w_continuation_special_key_nests_angle_notation() {
        assert_eq!(
            ctrl_w_continuation(Key::Escape, Modifiers::NONE),
            Some("<C-w><Esc>".to_string())
        );
    }

    #[test]
    fn single_char_handles_letters_and_digits_lowercased() {
        assert_eq!(single_char(Key::A), Some('a'));
        assert_eq!(single_char(Key::Num1), Some('1'));
        assert_eq!(single_char(Key::Escape), None);
    }

    /// A real per-press egui event pair: the `Key` press egui always
    /// sends, plus (for printable keys) the companion `Text` event that
    /// carries the actual character -- and, since egui reports releases
    /// too, the matching `Key` release. This is the exact sequence a
    /// physical `Ctrl-w` press-then-release, or an unmodified letter
    /// press-then-release, produces; `process_nvim_events` has to handle
    /// it, not the idealized "one Key event per press" a naive test would
    /// use instead.
    fn press_and_release(key: Key, modifiers: Modifiers, text: Option<&str>) -> Vec<Event> {
        let mut events = vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }];
        if let Some(text) = text {
            events.push(Event::Text(text.to_string()));
        }
        events.push(Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        });
        events
    }

    #[test]
    fn ctrl_w_then_h_hops_left_at_boundary_across_real_event_sequence() {
        // Ctrl-w: Key-down(ctrl) + Key-up, no Text (ctrl suppresses it).
        let mut events = press_and_release(Key::W, Modifiers::CTRL, None);
        // h: Key-down + Text("h") + Key-up, exactly what egui emits for an
        // unmodified printable key.
        events.extend(press_and_release(Key::H, Modifiers::NONE, Some("h")));

        let (actions, pending) = process_nvim_events(&events, false);

        assert!(!pending, "chord must be disarmed after completing");
        assert_eq!(
            actions,
            vec![NvimAction::CtrlWBoundary {
                dir: "h",
                hop_left: true,
                forward_seq: "<C-w>h",
            }],
            "exactly one action: the boundary check -- no leaked literal 'h' input, \
             and the Ctrl-w release must not have disarmed the chord before h arrived"
        );
    }

    #[test]
    fn ctrl_w_then_arrow_left_hops_left_across_real_event_sequence() {
        let mut events = press_and_release(Key::W, Modifiers::CTRL, None);
        // ArrowLeft: Key-down + Key-up, no Text (arrows never produce text).
        events.extend(press_and_release(Key::ArrowLeft, Modifiers::NONE, None));

        let (actions, pending) = process_nvim_events(&events, false);

        assert!(!pending);
        assert_eq!(
            actions,
            vec![NvimAction::CtrlWBoundary {
                dir: "h",
                hop_left: true,
                forward_seq: "<C-w><Left>",
            }]
        );
    }

    #[test]
    fn ctrl_w_then_l_across_real_event_sequence() {
        let mut events = press_and_release(Key::W, Modifiers::CTRL, None);
        events.extend(press_and_release(Key::L, Modifiers::NONE, Some("l")));

        let (actions, pending) = process_nvim_events(&events, false);

        assert!(!pending);
        assert_eq!(
            actions,
            vec![NvimAction::CtrlWBoundary {
                dir: "l",
                hop_left: false,
                forward_seq: "<C-w>l",
            }]
        );
    }

    #[test]
    fn ctrl_w_then_q_forwards_continuation_across_real_event_sequence() {
        let mut events = press_and_release(Key::W, Modifiers::CTRL, None);
        events.extend(press_and_release(Key::Q, Modifiers::NONE, Some("q")));

        let (actions, pending) = process_nvim_events(&events, false);

        assert!(!pending);
        assert_eq!(actions, vec![NvimAction::Input("<C-w>q".to_string())]);
    }

    #[test]
    fn ctrl_w_arm_alone_produces_no_action_and_stays_pending_into_next_frame() {
        // The Ctrl-w press+release can land in one frame with the
        // completing key arriving only on the *next* frame's event batch --
        // pending must carry across that boundary.
        let events = press_and_release(Key::W, Modifiers::CTRL, None);
        let (actions, pending) = process_nvim_events(&events, false);
        assert_eq!(actions, vec![]);
        assert!(pending, "chord must stay armed across the frame boundary");
    }

    #[test]
    fn plain_h_with_no_pending_chord_forwards_normally() {
        let events = press_and_release(Key::H, Modifiers::NONE, Some("h"));
        let (actions, pending) = process_nvim_events(&events, false);
        assert!(!pending);
        assert_eq!(actions, vec![NvimAction::Input("h".to_string())]);
    }

    #[test]
    fn ctrl_w_then_h_does_not_leak_the_paired_text_event_even_if_it_arrives_first() {
        // Defends the order-independence claim in `process_nvim_events`'s
        // doc: if Text("h") somehow arrives before its Key press within
        // the armed frame, it must be ignored (not treated as a
        // completion, not forwarded) rather than disarming or leaking.
        let mut events = press_and_release(Key::W, Modifiers::CTRL, None);
        events.push(Event::Text("h".to_string()));
        events.push(Event::Key {
            key: Key::H,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        });

        let (actions, pending) = process_nvim_events(&events, false);

        assert!(!pending);
        assert_eq!(
            actions,
            vec![NvimAction::CtrlWBoundary {
                dir: "h",
                hop_left: true,
                forward_seq: "<C-w>h",
            }],
            "no leaked literal 'h', chord still completes on the Key press"
        );
    }

    #[test]
    fn chord_persists_across_multiple_release_events_before_the_real_completion() {
        // Both halves of the chord can generate more than one release-ish
        // event in pathological orderings -- none of them should disarm.
        let events = vec![
            Event::Key {
                key: Key::W,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
            Event::Key {
                key: Key::W,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
            Event::Key {
                key: Key::W,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ];
        let (actions, pending) = process_nvim_events(&events, false);
        assert_eq!(actions, vec![]);
        assert!(pending);
    }
}
