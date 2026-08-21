//! `--tui`: the ratatui/crossterm terminal frontend (issue #16), phase 1 --
//! a focused-neighborhood view of one module at a time (the module plus
//! its direct dependencies/dependents), not a ported graph canvas. See the
//! issue for the full design rationale (no TUI ecosystem has a production
//! nested-DAG widget; this instead follows the `nix-tree` pattern, which
//! matches vdiff's own "one frame of the call-stack story at a time"
//! framing).
//!
//! This module is glue only, the same discipline `crate::ui` follows for
//! the GUI: all state and transition logic lives in
//! [`crate::core::app::App`]/[`crate::core::app::update`], reused entirely
//! unchanged -- `h`/`j`/`k`/`l` navigation, `gd`/`gr` edge-following (with
//! the same picker-when-ambiguous overlay), the diff/file panes, review
//! toggling, all of it. What's genuinely new here is: terminal setup/
//! teardown ([`run`]), the crossterm event loop ([`event_loop`]), the
//! crossterm-to-[`crate::keymap::KeyInput`] mapping ([`keys`]), a
//! `ratatui`-only [`Cmd::LoadDiff`]/[`Cmd::LoadFile`] IO glue
//! ([`loader`]) parallel to (but independent of) the GUI's
//! `crate::ui::eframe_app::DiffLoader`, the rendering itself ([`render`]),
//! a direct `syntect` -> `ratatui::style::Style` mapping ([`highlight`])
//! since there's no `egui_extras` to route through here, and the lazygit-
//! style real-`nvim` alternate-screen handoff ([`nvim_handoff`]) -- no
//! embedded `nvim --embed`/`ext_linegrid` grid; that stays GUI-only.
//!
//! Event-driven, not per-frame polled: [`event_loop`] blocks on
//! `crossterm::event::poll` and only redraws on an actual state change (a
//! dispatched message) or the periodic tick used for `--smoke`'s self-close
//! timer, never on a fixed frame-rate timer the way the eframe app's
//! `request_repaint_after` loop can.

pub mod highlight;
pub mod keys;
pub mod loader;
pub mod nvim_handoff;
pub mod render;

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::core::app::{update, App, Cmd, Msg, Pane};
use crate::keymap::{map_key, KeyContext, KeyInput, KeyOutcome, Pending};
use crate::review::review_state::ReviewStore;
use crate::review::store as review_store;
use loader::TuiLoader;

/// How long `--smoke` keeps the terminal open before exiting 0 -- mirrors
/// the GUI's own `SMOKE_DURATION` (`crate::ui::eframe_app`).
const SMOKE_DURATION: Duration = Duration::from_secs(2);

/// How long [`event_loop`]'s `crossterm::event::poll` blocks for between
/// checks of the `--smoke` timer. Small enough that `--smoke` closes
/// promptly; large enough that idling burns effectively no CPU (this is
/// event-driven, not a frame-rate poll -- see the module doc).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Everything [`run`] needs beyond the already-constructed [`App`]: the IO
/// loader for `Cmd::LoadDiff`/`Cmd::LoadFile`, the review-completion store/
/// branch to persist [`Cmd::PersistReviewState`] into, the repo root
/// (`nvim`'s working directory for [`nvim_handoff::suspend_and_run`]), and
/// `--smoke`.
pub struct TuiConfig {
    pub loader: TuiLoader,
    pub review_store: ReviewStore,
    pub review_branch: String,
    pub repo_root: PathBuf,
    pub smoke: bool,
}

/// Owns [`App`] and everything [`TuiConfig`] carried in, driving the
/// dispatch/execute loop [`crate::ui::eframe_app::VdiffApp`] documents for
/// the GUI -- the terminal-side equivalent of that struct, with no egui
/// anywhere in it.
struct TuiState {
    app: App,
    pending_key: Option<Pending>,
    loader: TuiLoader,
    review_store: ReviewStore,
    review_branch: String,
    repo_root: PathBuf,
}

impl TuiState {
    fn dispatch(&mut self, msg: Msg) {
        let (app, cmd) = update(self.app.clone(), msg);
        self.app = app;
        self.execute(cmd);
    }

    /// Execute a [`Cmd`]. `Cmd::Relayout` is a no-op here: unlike the GUI
    /// (which recomputes pixel geometry for the graph canvas),
    /// `core::app::toggle_tests` already updates `App::layers`/`App::rows`
    /// itself before returning it -- the neighborhood view never consults
    /// layout rects at all, only `App::graph`/`App::layers` directly (see
    /// `render::draw_neighborhood`), so there's nothing left for this glue
    /// to rebuild.
    fn execute(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::None | Cmd::Relayout => {}
            Cmd::LoadDiff(node) => match self.loader.load_diff(&self.app.graph, &node) {
                Ok(state) => self.dispatch(Msg::DiffLoaded(state)),
                Err(message) => self.dispatch(Msg::LoadFailed(message)),
            },
            Cmd::LoadFile(node) => match self.loader.load_file_view(&self.app.graph, &node) {
                Ok(state) => self.dispatch(Msg::FileLoaded(state)),
                Err(message) => self.dispatch(Msg::FileLoadFailed(message)),
            },
            Cmd::CommentNode(_) => {
                // Comment capture is a compose-UI feature the GUI delegates
                // to the embedded `vdiff.nvim` plugin (see
                // `crate::ui::eframe_app::VdiffApp::comment_node`); the TUI
                // has no embedded nvim session to delegate to at all (see
                // `nvim_handoff`'s doc on why phase 1 is handoff-only, not
                // embedded). Deferred rather than half-built here.
                eprintln!(
                    "note: capturing review comments isn't supported in the --tui frontend yet"
                );
            }
            Cmd::PersistReviewState => self.persist_review_state(),
        }
    }

    fn persist_review_state(&mut self) {
        let captured = crate::review::review_state::capture(&self.app.graph, &self.app.reviewed);
        self.review_store.set_branch(&self.review_branch, captured);
        let git_dir = self.loader.repo.git_dir();
        if let Err(err) = review_store::save_review_state(&git_dir, &self.review_store) {
            eprintln!(
                "warning: failed to save {}: {err}",
                review_store::review_state_path(&git_dir).display()
            );
        }
    }
}

/// What [`event_loop`] should do after handling one input event.
enum LoopAction {
    Continue,
    Quit,
}

/// Run the TUI to completion: enter raw mode/the alternate screen, drive
/// [`event_loop`] until it quits (`q`, or `--smoke`'s timer), then restore
/// the terminal unconditionally -- even if `event_loop` returned an error,
/// so a mid-run IO failure never leaves the caller's shell in raw mode with
/// no visible cursor.
pub fn run(app: App, config: TuiConfig) -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut state = TuiState {
        app,
        pending_key: None,
        loader: config.loader,
        review_store: config.review_store,
        review_branch: config.review_branch,
        repo_root: config.repo_root,
    };

    let result = event_loop(&mut terminal, &mut state, config.smoke);

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

/// The event loop itself: draw, then block on `crossterm::event::poll` for
/// up to [`POLL_INTERVAL`] (so the `--smoke` timer is checked periodically
/// even with no input), handling exactly one key event per wake before
/// looping back to redraw -- redraw-on-state-change, not a fixed frame
/// rate (see the module doc).
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut TuiState,
    smoke: bool,
) -> io::Result<()> {
    let started_at = Instant::now();
    loop {
        if state.app.pane == Pane::File {
            let size = terminal.size()?;
            state.app.viewport_rows = render::file_view_visible_rows(size.height);
        }
        terminal.draw(|frame| render::draw(frame, &state.app))?;

        if smoke && started_at.elapsed() > SMOKE_DURATION {
            return Ok(());
        }

        if !event::poll(POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let LoopAction::Quit = handle_key(terminal, state, key)? {
                    return Ok(());
                }
            }
            // Resize/other events need no explicit handling -- the next
            // loop iteration redraws unconditionally against whatever
            // `terminal.size()` reports then.
            _ => {}
        }
    }
}

/// Handle one keypress: `q` quits (unless the edge-picker overlay is open,
/// so `Esc` has first say over closing that instead), `Ctrl-e` on the file
/// pane suspends the TUI for a real `nvim` (see [`nvim_handoff`]), and
/// everything else is translated via [`keys::crossterm_key_to_input`] and
/// routed through [`map_key`]/the reducer exactly like the GUI's own
/// `handle_keys`.
fn handle_key(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut TuiState,
    key: KeyEvent,
) -> io::Result<LoopAction> {
    if key.code == KeyCode::Char('q') && state.app.picker.is_none() {
        return Ok(LoopAction::Quit);
    }

    let Some(input) = keys::crossterm_key_to_input(key.code, key.modifiers) else {
        return Ok(LoopAction::Continue);
    };

    if should_edit_in_nvim(state, input) {
        edit_in_nvim(terminal, state)?;
        return Ok(LoopAction::Continue);
    }

    let ctx = KeyContext {
        screen: state.app.screen,
        pane: state.app.pane,
        file_open: state.app.file_view.is_some(),
        picker_open: state.app.picker.is_some(),
        pending: state.pending_key,
    };
    let outcome = map_key(input, ctx);
    state.pending_key = None;
    match outcome {
        KeyOutcome::Msg(msg) => state.dispatch(msg),
        KeyOutcome::Pending(pending) => state.pending_key = Some(pending),
        KeyOutcome::None => {}
    }
    Ok(LoopAction::Continue)
}

/// Whether `input` should trigger [`edit_in_nvim`] instead of the normal
/// `map_key` pipeline: `Ctrl-e` on the file pane, with no chord in
/// progress (so a chord that happens to end in some future `e` binding, if
/// one is ever added, isn't shadowed).
fn should_edit_in_nvim(state: &TuiState, input: KeyInput) -> bool {
    input == KeyInput::Ctrl('e') && state.app.pane == Pane::File && state.pending_key.is_none()
}

/// `Ctrl-e`: suspend the TUI and open the file pane's current file in a
/// real `nvim`, at the line currently scrolled to the top of the pane (the
/// closest terminal-UI equivalent of "where the user is looking" without a
/// per-line cursor concept in [`crate::core::file_view::FileViewState`]).
/// Redraws immediately on return (`terminal.clear()`) since `nvim` painted
/// over the whole screen while it ran. A missing `nvim` binary or other
/// spawn failure is logged, not fatal -- the user stays in the TUI either
/// way.
fn edit_in_nvim(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &TuiState,
) -> io::Result<()> {
    let Some(file_view) = &state.app.file_view else {
        return Ok(());
    };
    let Some(file) = file_view.current_file() else {
        return Ok(());
    };
    let path = state.repo_root.join(&file.path);
    let line = Some(file_view.scroll_row as u32 + 1);
    if let Err(err) = nvim_handoff::suspend_and_run(&path, line) {
        eprintln!("warning: failed to launch nvim: {err}");
    }
    terminal.clear()
}
