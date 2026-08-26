//! TUI-local wrapper around an embedded [`NvimSession`]: owns the session,
//! the cols/rows last sent to it (for resize debounce, mirroring the GUI's
//! `crate::ui::nvim_pane::NvimPane`), the path most recently opened via
//! graph navigation (`:VdiffDiff`/`d`'s fallback -- see
//! [`crate::nvim::vdiff_glue::trigger_diffsplit`]), and the `Ctrl-w` chord's
//! armed/disarmed state (the TUI's counterpart of the GUI's
//! `VdiffApp::nvim_ctrl_w_pending`, kept on this struct instead since the
//! TUI's `handle_key` has no other natural home for it).
//!
//! The impure half (spawning, sending commands, RPC calls) lives on
//! [`NvimPane`] itself; the decisions -- what a keypress should do, whether
//! a resize needs to be sent -- are pure functions ([`route_key`],
//! [`resize_needed`]) so they can be unit-tested without a real terminal or
//! a spawned `nvim` process, per this crate's hard rule against tests that
//! need either.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::core::app::Pane;
use crate::nvim::grid::GridState;
use crate::nvim::session::{NvimCmd, NvimSession};
use crate::nvim::vdiff_glue;
use crate::tui::nvim_keys::key_event_to_nvim_input;

/// How long a boundary-detection [`NvimPane::at_boundary`] call (or the
/// diffsplit flow's current-buffer query) blocks the calling (event-loop)
/// thread before giving up -- mirrors the GUI's own
/// `crate::ui::nvim_pane::CALL_TIMEOUT` exactly, for the same reason: short
/// enough that a wedged-but-not-dead nvim can never make `Ctrl-w`
/// navigation (or `d`) feel stuck.
pub const CALL_TIMEOUT: Duration = Duration::from_millis(100);

/// Owns a live [`NvimSession`] plus everything the TUI's event loop/render
/// path needs alongside it. See the module doc for why each field lives
/// here rather than on `TuiState` directly.
pub struct NvimPane {
    session: NvimSession,
    cols: u16,
    rows: u16,
    /// The path most recently opened via graph navigation (`Cmd::LoadFile`),
    /// tracked as `:VdiffDiff`/`d`'s fallback target -- see
    /// [`vdiff_glue::trigger_diffsplit`]'s doc for why this is only ever a
    /// fallback, never trusted over nvim's actual current buffer.
    current_file: Option<PathBuf>,
    /// Whether the previous keypress (while this pane had focus) was
    /// `Ctrl-w`, awaiting a completing key -- see [`route_key`].
    ctrl_w_pending: bool,
}

impl NvimPane {
    /// Spawn `nvim --embed` in `cwd`, sized to `cols`x`rows`, and register
    /// the `:VdiffDiff`/`:VdiffDiffOff` commands. `repaint` is forwarded
    /// straight to [`NvimSession::spawn`] -- called from the reader thread
    /// after every flushed redraw batch.
    pub fn spawn(
        cwd: &Path,
        cols: u16,
        rows: u16,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let session = NvimSession::spawn(cwd, cols, rows, repaint)?;
        vdiff_glue::register_vdiff_commands(&session);
        Ok(NvimPane {
            session,
            cols,
            rows,
            current_file: None,
            ctrl_w_pending: false,
        })
    }

    /// Run one `--nvim-cmd` Ex command in this session, blocking up to
    /// [`CALL_TIMEOUT`] for nvim's reply -- the TUI's counterpart of
    /// `crate::ui::nvim_pane::NvimPane::run_init_command`, and a
    /// [`NvimSession::call`] rather than a fire-and-forget
    /// [`NvimCmd::Ex`] for the same reason: these are *user*-supplied
    /// commands (a typo'd `:ContextWindowHide`, a command from a plugin
    /// that isn't installed), so the caller needs to be able to surface the
    /// failure instead of leaving the user wondering why nothing happened.
    /// `Err` carries a ready-to-display message.
    pub fn run_init_command(&self, command: &str) -> Result<(), String> {
        match self.session.call(
            "nvim_command",
            vec![rmpv::Value::from(command)],
            CALL_TIMEOUT,
        ) {
            Some(_) => Ok(()),
            None => Err(format!(
                "nvim-cmd '{command}' failed or timed out (no response within {CALL_TIMEOUT:?})"
            )),
        }
    }

    /// Whether the underlying session still believes `nvim` is alive (see
    /// [`NvimSession::is_alive`]).
    pub fn is_alive(&self) -> bool {
        self.session.is_alive()
    }

    /// The shared grid, for the renderer to lock and paint from.
    pub fn grid(&self) -> Arc<Mutex<GridState>> {
        self.session.grid()
    }

    /// Forward a raw command to the session.
    pub fn send(&self, cmd: NvimCmd) {
        self.session.send(cmd);
    }

    /// The cols/rows last sent to nvim.
    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Resize the nvim UI to `new_cols`x`new_rows` if [`resize_needed`]
    /// says the pane's dimensions actually changed since the last send.
    pub fn maybe_resize(&mut self, new_cols: u16, new_rows: u16) {
        if resize_needed((self.cols, self.rows), (new_cols, new_rows)) {
            self.cols = new_cols;
            self.rows = new_rows;
            self.session.send(NvimCmd::Resize(new_cols, new_rows));
        }
    }

    /// Open `path` at `line` (1-based), marking every changed-head-line
    /// range in `ranges` -- see [`NvimCmd::OpenFile`]'s doc. Records `path`
    /// as [`Self::current_file`] for `:VdiffDiff`/`d`'s fallback.
    pub fn open_file(&mut self, path: PathBuf, line: Option<u64>, ranges: Vec<(usize, usize)>) {
        self.current_file = Some(path.clone());
        self.session.send(NvimCmd::OpenFile { path, line, ranges });
    }

    /// The path most recently opened via graph navigation, if any -- see
    /// [`Self::current_file`]'s own doc.
    pub fn current_file(&self) -> Option<PathBuf> {
        self.current_file.clone()
    }

    /// Whether at least one `:VdiffDiff` invocation arrived since the last
    /// call -- see [`NvimSession::take_diff_request`].
    pub fn take_diff_request(&self) -> bool {
        self.session.take_diff_request()
    }

    /// The diffsplit-against-merge-base flow -- see
    /// [`vdiff_glue::trigger_diffsplit`] for the full semantics.
    /// [`Self::current_file`] is the fallback consulted only when nvim's
    /// own current-buffer query can't produce a usable answer.
    pub fn trigger_diffsplit(
        &self,
        cwd: &Path,
        base_content_for: impl FnOnce(&Path) -> Option<String>,
    ) -> bool {
        vdiff_glue::trigger_diffsplit(
            &self.session,
            cwd,
            CALL_TIMEOUT,
            self.current_file.clone(),
            base_content_for,
        )
    }

    /// Whether the current window is already at nvim's split boundary in
    /// direction `dir` -- see [`vdiff_glue::at_boundary`].
    pub fn at_boundary(&self, dir: &str) -> bool {
        vdiff_glue::at_boundary(&self.session, dir, CALL_TIMEOUT)
    }

    /// Whether a `Ctrl-w` chord is currently armed, awaiting its
    /// completing key -- see [`route_key`].
    pub fn ctrl_w_pending(&self) -> bool {
        self.ctrl_w_pending
    }

    /// Arm/disarm the `Ctrl-w` chord -- called from the event loop right
    /// after [`route_key`] reports [`KeyRoute::ArmCtrlW`] or resolves a
    /// pending chord.
    pub fn set_ctrl_w_pending(&mut self, pending: bool) {
        self.ctrl_w_pending = pending;
    }
}

/// Whether `new` differs from `last` (and isn't degenerate -- a `0` in
/// either dimension is never sent, matching the GUI's own
/// `NvimPane::maybe_resize` guard: a transiently zero-sized pane, e.g.
/// mid-terminal-resize, shouldn't attach nvim's UI at `0x0`).
pub fn resize_needed(last: (u16, u16), new: (u16, u16)) -> bool {
    new.0 != 0 && new.1 != 0 && new != last
}

/// One decision [`route_key`] produces for a keypress that landed while the
/// file pane shows the embedded nvim session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRoute {
    /// Send this nvim key notation via [`NvimCmd::Input`].
    ForwardToNvim(String),
    /// `Ctrl-w` itself: arm the chord for the next keypress.
    ArmCtrlW,
    /// A completed `Ctrl-w h`/`Ctrl-w l` (or arrow alias): the caller
    /// queries [`NvimPane::at_boundary`]; at the boundary, dispatch
    /// `Msg::PaneLeft` if `hop_left` (a no-op at the right boundary --
    /// there's no pane further right than the nvim pane); otherwise send
    /// `forward_seq` so nvim moves between its own splits.
    CtrlWBoundary {
        dir: &'static str,
        hop_left: bool,
        forward_seq: &'static str,
    },
    /// The key was nvim's to handle, but translated to no actual input
    /// (e.g. a key-release event, or an unmapped code) -- consumed as a
    /// no-op, distinct from [`KeyRoute::HandleNormally`]: this key was
    /// still swallowed by nvim routing, not left for the global handlers.
    Consumed,
    /// Not nvim's territory at all (wrong pane, or no live session) --
    /// the caller should fall through to the existing `handle_key` logic
    /// unchanged.
    HandleNormally,
}

/// The routing decision for `key`, given `pane`/`nvim_alive`/
/// `ctrl_w_pending`. Pure: no session, no I/O, no terminal -- see the
/// module doc.
///
/// `pane != Pane::File` or `!nvim_alive` always means
/// [`KeyRoute::HandleNormally`] -- nvim only ever owns the file pane, and
/// only while actually alive (a dead session degrades to the hand-rolled
/// viewers, per the crate's fallback rule). Otherwise: a `Ctrl-w` chord in
/// progress resolves via [`KeyRoute::CtrlWBoundary`] for `h`/`l`/arrow-left/
/// arrow-right, or `<C-w><key>` for anything else (restoring the rest of
/// nvim's own `Ctrl-w` repertoire -- `q`, `o`, `w`, ... -- rather than
/// silently dropping it, mirroring the GUI's `ctrl_w_continuation`); a bare
/// `Ctrl-w` arms the chord; everything else translates via
/// [`key_event_to_nvim_input`] and forwards as-is -- including `q`, `Ctrl-e`,
/// and `c`, all of which mean something entirely different to nvim than to
/// vdiff's own global handlers.
pub fn route_key(pane: Pane, nvim_alive: bool, ctrl_w_pending: bool, key: &KeyEvent) -> KeyRoute {
    if pane != Pane::File || !nvim_alive {
        return KeyRoute::HandleNormally;
    }

    if ctrl_w_pending {
        return match key.code {
            KeyCode::Char('h') => KeyRoute::CtrlWBoundary {
                dir: "h",
                hop_left: true,
                forward_seq: "<C-w>h",
            },
            KeyCode::Left => KeyRoute::CtrlWBoundary {
                dir: "h",
                hop_left: true,
                forward_seq: "<C-w><Left>",
            },
            KeyCode::Char('l') => KeyRoute::CtrlWBoundary {
                dir: "l",
                hop_left: false,
                forward_seq: "<C-w>l",
            },
            KeyCode::Right => KeyRoute::CtrlWBoundary {
                dir: "l",
                hop_left: false,
                forward_seq: "<C-w><Right>",
            },
            _ => match key_event_to_nvim_input(key) {
                Some(seq) => KeyRoute::ForwardToNvim(format!("<C-w>{seq}")),
                None => KeyRoute::Consumed,
            },
        };
    }

    if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return KeyRoute::ArmCtrlW;
    }

    match key_event_to_nvim_input(key) {
        Some(seq) => KeyRoute::ForwardToNvim(seq),
        None => KeyRoute::Consumed,
    }
}

/// Whether a fresh session needs spawning before the file pane is handed
/// to nvim: this run has one (nvim mode is on and the initial spawn
/// succeeded) but the process behind it is gone -- the user `ZZ`'d or
/// `:q`'d out of it. Pure counterpart of `TuiState::ensure_nvim_session`,
/// which does the actual respawn; `false` when there was never a session
/// to begin with (`--no-nvim`, no binary, a failed initial spawn), since
/// that's this run's standing decision to use the built-in viewers rather
/// than a session to recover.
pub fn respawn_needed(present: bool, alive: bool) -> bool {
    present && !alive
}

/// Whether the graph pane's `d` binding should open the focused node's file
/// in nvim plus its diffsplit (the GUI's nvim-mode `d`), instead of
/// dispatching the hand-rolled `Msg::OpenDiff` full-screen diff view: nvim
/// present, on [`crate::core::app::Screen::Graph`]/[`Pane::Graph`] (never
/// [`Pane::File`] -- that's [`route_key`]'s territory, where `d` forwards to
/// nvim as `dd`/etc. instead), with no picker or chord mid-flight (so `gd`'s
/// `d`, or any other multi-key sequence ending in `d`, is untouched). Pure
/// given the caller's already-extracted state, mirroring the GUI's
/// `VdiffApp::should_open_nvim_diff` exactly.
///
/// `nvim_present` is deliberately *presence*, not liveness (unlike
/// [`route_key`]'s `nvim_alive`, which must stay liveness -- a dead session
/// may never swallow a keypress): `d` is one of the two keys that opens the
/// file pane, so a session the user quit out of is respawned at that point
/// (see [`respawn_needed`]) rather than permanently downgrading the rest of
/// the run to the built-in diff view. The caller falls back to the
/// hand-rolled view anyway if that respawn fails.
pub fn should_open_diff_in_nvim(
    nvim_present: bool,
    screen: crate::core::app::Screen,
    pane: Pane,
    picker_open: bool,
    chord_pending: bool,
) -> bool {
    nvim_present
        && screen == crate::core::app::Screen::Graph
        && pane == Pane::Graph
        && !picker_open
        && !chord_pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    // -- resize_needed ------------------------------------------------------

    #[test]
    fn resize_needed_when_dims_change() {
        assert!(resize_needed((80, 24), (100, 30)));
    }

    #[test]
    fn resize_not_needed_when_dims_unchanged() {
        assert!(!resize_needed((80, 24), (80, 24)));
    }

    #[test]
    fn resize_not_needed_for_a_zero_dimension() {
        assert!(!resize_needed((80, 24), (0, 30)));
        assert!(!resize_needed((80, 24), (30, 0)));
    }

    // -- route_key: only the file pane with a live session is nvim's ------

    #[test]
    fn graph_pane_is_never_routed_to_nvim() {
        let route = route_key(
            Pane::Graph,
            true,
            false,
            &key(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert_eq!(route, KeyRoute::HandleNormally);
    }

    #[test]
    fn a_dead_session_hands_every_key_back_to_normal_handling() {
        let route = route_key(
            Pane::File,
            false,
            false,
            &key(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert_eq!(route, KeyRoute::HandleNormally);
    }

    // -- q: forwards to nvim when alive, otherwise the caller's own q-quits -

    #[test]
    fn q_on_the_file_pane_forwards_to_nvim_when_alive() {
        let route = route_key(
            Pane::File,
            true,
            false,
            &key(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert_eq!(route, KeyRoute::ForwardToNvim("q".to_string()));
    }

    #[test]
    fn q_on_the_file_pane_is_handled_normally_when_nvim_is_dead() {
        let route = route_key(
            Pane::File,
            false,
            false,
            &key(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert_eq!(route, KeyRoute::HandleNormally);
    }

    // -- Ctrl-e: forwards when alive, hands off (via HandleNormally, then
    // the caller's existing should_edit_in_nvim) when not ------------------

    #[test]
    fn ctrl_e_forwards_to_nvim_when_alive() {
        let route = route_key(
            Pane::File,
            true,
            false,
            &key(KeyCode::Char('e'), KeyModifiers::CONTROL),
        );
        assert_eq!(route, KeyRoute::ForwardToNvim("<C-e>".to_string()));
    }

    #[test]
    fn ctrl_e_is_handled_normally_when_nvim_is_dead() {
        let route = route_key(
            Pane::File,
            false,
            false,
            &key(KeyCode::Char('e'), KeyModifiers::CONTROL),
        );
        assert_eq!(route, KeyRoute::HandleNormally);
    }

    // -- Ctrl-w chord states ------------------------------------------------

    #[test]
    fn ctrl_w_arms_the_chord() {
        let route = route_key(
            Pane::File,
            true,
            false,
            &key(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(route, KeyRoute::ArmCtrlW);
    }

    #[test]
    fn ctrl_w_then_h_checks_the_left_boundary() {
        let route = route_key(
            Pane::File,
            true,
            true,
            &key(KeyCode::Char('h'), KeyModifiers::NONE),
        );
        assert_eq!(
            route,
            KeyRoute::CtrlWBoundary {
                dir: "h",
                hop_left: true,
                forward_seq: "<C-w>h",
            }
        );
    }

    #[test]
    fn ctrl_w_then_l_checks_the_right_boundary() {
        let route = route_key(
            Pane::File,
            true,
            true,
            &key(KeyCode::Char('l'), KeyModifiers::NONE),
        );
        assert_eq!(
            route,
            KeyRoute::CtrlWBoundary {
                dir: "l",
                hop_left: false,
                forward_seq: "<C-w>l",
            }
        );
    }

    #[test]
    fn ctrl_w_then_arrow_left_checks_the_left_boundary() {
        let route = route_key(
            Pane::File,
            true,
            true,
            &key(KeyCode::Left, KeyModifiers::NONE),
        );
        assert_eq!(
            route,
            KeyRoute::CtrlWBoundary {
                dir: "h",
                hop_left: true,
                forward_seq: "<C-w><Left>",
            }
        );
    }

    #[test]
    fn ctrl_w_then_an_unrelated_key_forwards_the_full_chord() {
        let route = route_key(
            Pane::File,
            true,
            true,
            &key(KeyCode::Char('q'), KeyModifiers::NONE),
        );
        assert_eq!(route, KeyRoute::ForwardToNvim("<C-w>q".to_string()));
    }

    #[test]
    fn ctrl_w_then_ctrl_w_forwards_the_nested_chord() {
        let route = route_key(
            Pane::File,
            true,
            true,
            &key(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(route, KeyRoute::ForwardToNvim("<C-w><C-w>".to_string()));
    }

    // -- respawn_needed -----------------------------------------------------

    #[test]
    fn a_dead_session_needs_a_respawn() {
        assert!(respawn_needed(true, false));
    }

    #[test]
    fn a_live_session_needs_no_respawn() {
        assert!(!respawn_needed(true, true));
    }

    #[test]
    fn a_run_with_no_session_at_all_never_respawns() {
        assert!(!respawn_needed(false, false));
    }

    // -- should_open_diff_in_nvim: d-interception on/off by presence -------

    #[test]
    fn d_is_intercepted_on_the_graph_pane_when_nvim_is_present() {
        assert!(should_open_diff_in_nvim(
            true,
            crate::core::app::Screen::Graph,
            Pane::Graph,
            false,
            false,
        ));
    }

    #[test]
    fn d_is_not_intercepted_when_this_run_has_no_nvim_session() {
        assert!(!should_open_diff_in_nvim(
            false,
            crate::core::app::Screen::Graph,
            Pane::Graph,
            false,
            false,
        ));
    }

    #[test]
    fn d_is_not_intercepted_on_the_file_pane() {
        assert!(!should_open_diff_in_nvim(
            true,
            crate::core::app::Screen::Graph,
            Pane::File,
            false,
            false,
        ));
    }

    #[test]
    fn d_is_not_intercepted_with_a_picker_open() {
        assert!(!should_open_diff_in_nvim(
            true,
            crate::core::app::Screen::Graph,
            Pane::Graph,
            true,
            false,
        ));
    }

    #[test]
    fn d_is_not_intercepted_mid_chord() {
        assert!(!should_open_diff_in_nvim(
            true,
            crate::core::app::Screen::Graph,
            Pane::Graph,
            false,
            true,
        ));
    }
}
