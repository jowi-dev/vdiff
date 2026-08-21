//! `--tui`: the ratatui/crossterm terminal frontend (issue #16). Phase 1
//! shipped a focused-neighborhood view of one module at a time; real use
//! found it "less helpful than just looking at the directory directly" --
//! no big picture, no zoom. Phase 2 (this version) replaces the graph
//! screen with a `git log --graph`-style vertical rail DAG instead: every
//! visible module as one row, a rail gutter drawing dependency edges
//! between them (see [`crate::graph::rails`]), and fold-by-namespace as the
//! zoom mechanic (see [`crate::core::rail_view`]). See [`render`]'s module
//! doc for the rendering side of that redesign.
//!
//! This module is glue only, the same discipline `crate::ui` follows for
//! the GUI: all state and transition logic lives in
//! [`crate::core::app::App`]/[`crate::core::app::update`]. `gd`/`gr` edge-
//! following (with the same picker-when-ambiguous overlay), the diff/file
//! panes, review toggling, `t`/`gt`/`c` -- all reused entirely unchanged
//! from the GUI's own reducer, still routed through the shared
//! [`crate::keymap::map_key`]. `h`/`j`/`k`/`l` are the one exception: the
//! rail view's `j`/`k` (move down/up the fold-aware visible row list) and
//! `h`/`l` (collapse/expand a namespace) are semantically nothing like the
//! GUI's layer-grid `h`/`j`/`k`/`l` (which `map_key` still serves
//! unchanged, since `map_key`/`KeyContext` are shared with the GUI and
//! must keep working there) -- so [`handle_key`] intercepts these four keys
//! directly, before they ever reach `map_key`, dispatching
//! [`crate::core::app::Msg::RailFocusMove`]/
//! [`crate::core::app::Msg::CollapseFocusedNamespace`]/
//! [`crate::core::app::Msg::ExpandFocusedNamespace`] instead. This mirrors
//! the existing precedent for `Ctrl-e` (see [`should_edit_in_nvim`]):
//! anything that means something different in the TUI than in the GUI
//! bypasses the shared reducer's shared keymap entirely rather than
//! growing `map_key` a context flag to disambiguate. Every other binding
//! (`gd`/`gr`, `gt`, `t`, `v`, `c`, `Enter`, `d`, `q`, `Esc`, the file/diff
//! panes' own chords) is untouched and still flows through `map_key`.
//!
//! What's genuinely new in this module: terminal setup/teardown ([`run`]),
//! the crossterm event loop ([`event_loop`]), the crossterm-to-
//! [`crate::keymap::KeyInput`] mapping ([`keys`]), a `ratatui`-only
//! [`Cmd::LoadDiff`]/[`Cmd::LoadFile`] IO glue ([`loader`]) parallel to (but
//! independent of) the GUI's `crate::ui::eframe_app::DiffLoader`, the
//! rendering itself ([`render`]), a direct `syntect` ->
//! `ratatui::style::Style` mapping ([`highlight`]) since there's no
//! `egui_extras` to route through here, and the lazygit-style real-`nvim`
//! alternate-screen handoff ([`nvim_handoff`]) -- no embedded `nvim --embed`/
//! `ext_linegrid` grid; that stays GUI-only.
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

use crate::core::app::{update, App, Cmd, Msg, Pane, Screen};
use crate::core::rail_view::RailDirection;
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
    /// A one-shot message for the legend strip, replacing the usual
    /// keymap hint until the next keypress clears it (see [`handle_key`]).
    /// Display-only glue state -- deliberately *not* on `core::App`: `core`
    /// has no status/error field (see `Msg::LoadFailed`'s own doc on that),
    /// and this is purely a terminal-UI workaround for the fact that
    /// `eprintln!` is invisible/garbled while the alternate screen owns the
    /// terminal (raw mode suppresses the normal line-buffered scrollback
    /// stderr would otherwise show up in). The GUI has no equivalent need
    /// -- its `eprintln!`s land in whatever terminal launched it, which
    /// isn't captured by the GUI window at all.
    notice: Option<String>,
    /// The rail view's current scroll offset (row index of the topmost
    /// visible row), reclamped every frame in [`event_loop`] via
    /// [`render::clamp_scroll`] before [`render::draw`] runs -- the same
    /// pattern already used for [`App::viewport_rows`]/
    /// [`render::file_view_visible_rows`]. Deliberately *not* on
    /// `core::App`: like [`Self::notice`], it's display-only bookkeeping a
    /// fold/focus change never needs to reason about directly -- every
    /// frame recomputes it fresh from `App::focus` and the current visible
    /// row list, so there's no state here a `core` reducer could get out
    /// of sync with.
    rail_scroll: usize,
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
    /// itself before returning it -- the rail view never consults layout
    /// rects at all, only `App::graph`/`App::layers` (by way of
    /// `crate::core::rail_view::visible_rows`, see `render::draw_rail_graph`),
    /// so there's nothing left for this glue to rebuild.
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
                // embedded). Deferred rather than half-built here -- surfaced
                // via `self.notice` (see its doc), not `eprintln!`, since
                // stderr is invisible while the alternate screen owns the
                // terminal; without this, pressing `c` would look like a
                // dead key.
                self.notice =
                    Some("comments aren't supported in the --tui frontend yet".to_string());
            }
            Cmd::PersistReviewState => self.persist_review_state(),
        }
    }

    fn persist_review_state(&mut self) {
        let captured = crate::review::review_state::capture(&self.app.graph, &self.app.reviewed);
        self.review_store.set_branch(&self.review_branch, captured);
        let git_dir = self.loader.repo.git_dir();
        if let Err(err) = review_store::save_review_state(&git_dir, &self.review_store) {
            // See `Self::notice`'s doc: `eprintln!` would be invisible here,
            // same reason as `Cmd::CommentNode`'s note above.
            self.notice = Some(format!(
                "warning: failed to save {}: {err}",
                review_store::review_state_path(&git_dir).display()
            ));
        }
    }
}

/// What [`event_loop`] should do after [`handle_key`] processes one
/// keypress: keep going, quit, or -- the one case that needs the real
/// terminal, which [`handle_key`] itself deliberately never touches (see
/// its doc) -- suspend for a real `nvim` at `path`/`line`.
enum KeyAction {
    Continue,
    Quit,
    EditInNvim { path: PathBuf, line: Option<u32> },
}

/// Best-effort terminal restore: disable raw mode and leave the alternate
/// screen, ignoring either call's own error -- deliberately not `?`-chained
/// (unlike the pre-fix version of [`run`]'s teardown), since a failure in
/// `disable_raw_mode` must not skip the `LeaveAlternateScreen` attempt that
/// follows it. Used both by [`run`]'s normal exit path and by the panic
/// hook [`install_panic_hook`] installs, so a panic anywhere in
/// [`event_loop`]/[`render`]/dispatch still leaves the caller's shell in a
/// normal, readable state instead of wedged in raw mode with the panic
/// message swallowed by the alternate screen.
fn restore_terminal_best_effort() {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
}

/// Install a panic hook that restores the terminal (see
/// [`restore_terminal_best_effort`]) before delegating to whatever hook was
/// previously installed -- the standard library's default one, printing the
/// message and location, unless some outer caller already replaced it.
/// Installed once per [`run`] call, before `enable_raw_mode` is even
/// called, so a panic during terminal setup itself is covered too, not
/// just ones inside the event loop.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_best_effort();
        previous(info);
    }));
}

/// Run the TUI to completion: install the panic hook, enter raw mode/the
/// alternate screen, drive [`event_loop`] until it quits (`q`, or
/// `--smoke`'s timer), then restore the terminal unconditionally -- even if
/// `event_loop` returned an error, so a mid-run IO failure never leaves the
/// caller's shell in raw mode with no visible cursor. The panic hook
/// installed here covers the crash case this normal-exit teardown can't:
/// if `event_loop`/`render` panics instead of erroring, unwinding skips
/// straight past this function's own `restore_terminal_best_effort()` call
/// (there's no `catch_unwind` here), so the hook is what actually restores
/// the terminal in that case -- the two paths are complementary, not
/// redundant, so there's no double-restore to worry about: at most one of
/// them ever runs for a given call to `run`.
pub fn run(app: App, config: TuiConfig) -> io::Result<()> {
    install_panic_hook();
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
        notice: None,
        rail_scroll: 0,
    };

    let result = event_loop(&mut terminal, &mut state, config.smoke);

    restore_terminal_best_effort();

    result
}

/// The event loop itself: draw, then block on `crossterm::event::poll` for
/// up to [`POLL_INTERVAL`] (so the `--smoke` timer is checked periodically
/// even with no input), handling exactly one key event per wake before
/// looping back to redraw -- redraw-on-state-change, not a fixed frame
/// rate (see the module doc). [`KeyAction::EditInNvim`] is the one case
/// [`handle_key`] can't finish on its own (it needs the real
/// `Terminal`, which `handle_key` is deliberately kept free of -- see that
/// function's doc), so this is where [`nvim_handoff::suspend_and_run`] and
/// the post-return `terminal.clear()` actually happen.
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
        if state.app.screen == Screen::Graph && state.app.pane == Pane::Graph {
            let size = terminal.size()?;
            let viewport_height = render::rail_visible_rows(size.height);
            let rows = crate::core::rail_view::visible_rows(
                &state.app.graph,
                &state.app.layers,
                &state.app.fold_collapsed,
            );
            let focus_idx = rows
                .iter()
                .position(|row| row.id() == &state.app.focus)
                .unwrap_or(0);
            state.rail_scroll =
                render::clamp_scroll(state.rail_scroll, focus_idx, rows.len(), viewport_height);
        }
        terminal.draw(|frame| {
            render::draw(
                frame,
                &state.app,
                state.notice.as_deref(),
                state.rail_scroll,
            )
        })?;

        if smoke && started_at.elapsed() > SMOKE_DURATION {
            return Ok(());
        }

        if !event::poll(POLL_INTERVAL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match handle_key(state, key) {
                    KeyAction::Quit => return Ok(()),
                    KeyAction::EditInNvim { path, line } => {
                        if let Err(err) = nvim_handoff::suspend_and_run(&path, line) {
                            // See `TuiState::notice`'s doc: `eprintln!`
                            // would be invisible/garbled while the
                            // alternate screen still owns the terminal at
                            // this point (the handoff already restored and
                            // re-suspended it around the `nvim` child, but
                            // this warning fires after that, back under
                            // our own alternate screen).
                            state.notice = Some(format!("failed to launch nvim: {err}"));
                        }
                        terminal.clear()?;
                    }
                    KeyAction::Continue => {}
                }
            }
            // Resize/other events need no explicit handling -- the next
            // loop iteration redraws unconditionally against whatever
            // `terminal.size()` reports then.
            _ => {}
        }
    }
}

/// Handle one keypress and decide what [`event_loop`] should do next --
/// deliberately takes no `Terminal` at all, so it (and therefore the
/// [`TuiState::notice`] mechanics it drives) can be exercised head-on in a
/// unit test without any real terminal in play, per this crate's hard rule
/// against tests that need one. Clears [`TuiState::notice`] first, before
/// any dispatch -- so a notice shows for exactly one render (the one right
/// after it was set) and is gone by the next keypress, matching a toast/
/// status-line convention rather than lingering until something else
/// happens to overwrite it.
///
/// `q` quits (unless the edge-picker overlay is open, so `Esc` has first
/// say over closing that instead); `Ctrl-e` on the file pane requests
/// [`KeyAction::EditInNvim`] instead of dispatching through `map_key` (see
/// [`should_edit_in_nvim`]); `h`/`j`/`k`/`l` on the rail view (see
/// [`rail_key_msg`]) dispatch the rail-specific messages directly, bypassing
/// `map_key` entirely -- see this module's own doc for why (in short:
/// `map_key` is shared with the GUI, whose `h`/`j`/`k`/`l` must keep their
/// existing layer-grid meaning); everything else is translated via
/// [`keys::crossterm_key_to_input`] and routed through [`map_key`]/the
/// reducer exactly like the GUI's own `handle_keys`.
fn handle_key(state: &mut TuiState, key: KeyEvent) -> KeyAction {
    state.notice = None;

    if key.code == KeyCode::Char('q') && state.app.picker.is_none() {
        return KeyAction::Quit;
    }

    let Some(input) = keys::crossterm_key_to_input(key.code, key.modifiers) else {
        return KeyAction::Continue;
    };

    if should_edit_in_nvim(state, input) {
        return nvim_edit_target(state)
            .map(|(path, line)| KeyAction::EditInNvim { path, line })
            .unwrap_or(KeyAction::Continue);
    }

    if let Some(msg) = rail_key_msg(state, input) {
        state.dispatch(msg);
        return KeyAction::Continue;
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
        KeyOutcome::Msg(Msg::OpenFile) if !node_has_files(&state.app, &state.app.focus) => {
            state.notice = Some(FILE_LESS_ROW_NOTICE.to_string());
        }
        KeyOutcome::Msg(Msg::OpenDiff) if !node_has_files(&state.app, &diff_target(&state.app)) => {
            state.notice = Some(FILE_LESS_ROW_NOTICE.to_string());
        }
        KeyOutcome::Msg(msg) => state.dispatch(msg),
        KeyOutcome::Pending(pending) => state.pending_key = Some(pending),
        KeyOutcome::None => {}
    }
    KeyAction::Continue
}

/// The notice shown when `Enter`/`d` on a collapsed namespace row is a
/// no-op (see [`node_has_files`]/[`diff_target`]) -- without this, that key
/// would look dead rather than explain itself, the same problem
/// [`TuiState::notice`]'s doc already solves for `Cmd::CommentNode`.
const FILE_LESS_ROW_NOTICE: &str = "collapsed namespace has no files -- expand with l";

/// Whether `id` has at least one backing file -- mirrors
/// `crate::core::app`'s own `has_files` guard on `Msg::OpenFile`/
/// `Msg::OpenDiff` exactly (duplicated rather than exported from `core`,
/// since `core` is IO/display-glue-free and has no business knowing this
/// is used to decide whether to show a notice). `handle_key` checks this
/// *before* dispatching so it can tell the no-op apart from every other
/// reason `Msg::OpenFile`/`Msg::OpenDiff` might already be a no-op (no
/// picker-closed/pane guard failure gets a notice -- only this one, which
/// is otherwise silent from `core`'s side since `Cmd::None` doesn't say
/// why).
fn node_has_files(app: &App, id: &crate::graph::model::NodeId) -> bool {
    app.graph
        .node(id)
        .is_some_and(|node| !node.files.is_empty())
}

/// The node `Msg::OpenDiff` would target, mirroring
/// `crate::core::app::open_diff`'s own target selection exactly
/// (`App::file_view`'s node while the file pane is open, `focus`
/// otherwise) -- duplicated here for the same reason as [`node_has_files`].
fn diff_target(app: &App) -> crate::graph::model::NodeId {
    match (&app.file_view, app.pane) {
        (Some(file_view), Pane::File) => file_view.node.clone(),
        _ => app.focus.clone(),
    }
}

/// Whether `input` should trigger [`KeyAction::EditInNvim`] instead of the
/// normal `map_key` pipeline: `Ctrl-e` on the file pane, with no chord in
/// progress (so a chord that happens to end in some future `e` binding, if
/// one is ever added, isn't shadowed).
fn should_edit_in_nvim(state: &TuiState, input: KeyInput) -> bool {
    input == KeyInput::Ctrl('e') && state.app.pane == Pane::File && state.pending_key.is_none()
}

/// The rail-view message `input` should dispatch directly, bypassing
/// `map_key`, or `None` if it isn't one of the four rail-specific keys, or
/// the context isn't right for them: [`Screen::Graph`]/[`Pane::Graph`] with
/// no picker open (the picker's own `j`/`k` selection-move must win instead
/// -- see `map_key`'s picker-open precedence) and no chord in progress
/// (`h`/`j`/`k`/`l` aren't chord characters themselves, but if some other
/// chord -- e.g. `g`+? -- is already pending, this key should complete or
/// clear *that* chord via the normal `map_key` path, not be hijacked here).
/// `j`/`k` map to [`Msg::RailFocusMove`] (down/up the visible row list);
/// `h`/`l` map to [`Msg::CollapseFocusedNamespace`]/
/// [`Msg::ExpandFocusedNamespace`].
fn rail_key_msg(state: &TuiState, input: KeyInput) -> Option<Msg> {
    if state.app.screen != Screen::Graph
        || state.app.pane != Pane::Graph
        || state.app.picker.is_some()
        || state.pending_key.is_some()
    {
        return None;
    }
    match input {
        KeyInput::Char('j') => Some(Msg::RailFocusMove(RailDirection::Down)),
        KeyInput::Char('k') => Some(Msg::RailFocusMove(RailDirection::Up)),
        KeyInput::Char('h') => Some(Msg::CollapseFocusedNamespace),
        KeyInput::Char('l') => Some(Msg::ExpandFocusedNamespace),
        _ => None,
    }
}

/// `Ctrl-e`'s target: the file pane's current file, joined onto the repo
/// root, at the line currently scrolled to the top of the pane (the closest
/// terminal-UI equivalent of "where the user is looking" without a
/// per-line cursor concept in [`crate::core::file_view::FileViewState`]).
/// `None` if there's no file pane open or it has no files -- shouldn't
/// happen given [`should_edit_in_nvim`]'s own `Pane::File` guard, but
/// [`handle_key`] treats it as a no-op rather than assuming.
fn nvim_edit_target(state: &TuiState) -> Option<(PathBuf, Option<u32>)> {
    let file_view = state.app.file_view.as_ref()?;
    let file = file_view.current_file()?;
    let path = state.repo_root.join(&file.path);
    let line = Some(file_view.scroll_row as u32 + 1);
    Some((path, line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::app::Screen;
    use crate::graph::model::ProjectGraph;
    use crate::pipeline::repo::FakeRepo;
    use crossterm::event::KeyModifiers;
    use std::collections::{HashMap, HashSet};

    /// A `TuiState` with an empty graph -- `handle_key`'s `Cmd::CommentNode`
    /// path only checks screen/pane/picker (see
    /// `core::app::on_graph_with_no_picker_and_graph_pane`), never looks up
    /// the focused node itself, so there's no need for a populated graph
    /// fixture here.
    fn state_fixture() -> TuiState {
        let app = App {
            graph: ProjectGraph {
                nodes: HashMap::new(),
                roots: vec![],
                edges: vec![],
            },
            layers: vec![],
            rows: vec![],
            focus: crate::graph::model::NodeId::from(""),
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
        };
        TuiState {
            app,
            pending_key: None,
            loader: TuiLoader {
                repo: Box::new(FakeRepo::default()),
                base_oid: "base-oid".to_string(),
            },
            review_store: ReviewStore::default(),
            review_branch: "main".to_string(),
            repo_root: PathBuf::from("."),
            notice: None,
            rail_scroll: 0,
        }
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn pressing_c_on_the_graph_pane_sets_a_notice_instead_of_dispatching_silently() {
        let mut state = state_fixture();
        let action = handle_key(&mut state, press('c'));
        assert!(matches!(action, KeyAction::Continue));
        assert!(
            state.notice.is_some(),
            "expected a notice explaining comments aren't supported yet"
        );
        assert!(state.notice.as_ref().unwrap().contains("comments"));
    }

    #[test]
    fn the_notice_renders_in_the_legend_strip() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut state = state_fixture();
        handle_key(&mut state, press('c'));
        let notice = state.notice.clone().expect("notice should be set");

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| {
                render::draw(
                    frame,
                    &state.app,
                    state.notice.as_deref(),
                    state.rail_scroll,
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(
            text.contains(&notice),
            "expected the legend strip to render the notice text, got: {text}"
        );
    }

    #[test]
    fn a_notice_is_cleared_by_the_next_keypress() {
        let mut state = state_fixture();
        handle_key(&mut state, press('c'));
        assert!(state.notice.is_some());

        // Any other keypress on the graph pane (here, an unmapped one)
        // clears the stale notice, per `handle_key`'s doc.
        handle_key(&mut state, press('x'));
        assert!(state.notice.is_none());
    }

    #[test]
    fn q_quits_when_no_picker_is_open() {
        let mut state = state_fixture();
        assert!(matches!(
            handle_key(&mut state, press('q')),
            KeyAction::Quit
        ));
    }

    #[test]
    fn q_is_swallowed_by_an_open_picker() {
        use crate::core::app::EdgePicker;
        let mut state = state_fixture();
        state.app.picker = Some(EdgePicker {
            candidates: vec![crate::graph::model::NodeId::from("x")],
            selected: 0,
        });
        assert!(matches!(
            handle_key(&mut state, press('q')),
            KeyAction::Continue
        ));
    }

    // -- Fix: file-less rows get a notice instead of a dead key (review
    // feedback) -----------------------------------------------------------

    /// `ns` is a synthetic, file-less namespace containing one drawn child
    /// `leaf` -- a stand-in for a collapsed namespace row's own id
    /// (`crate::core::rail_view::RailRow::Collapsed::namespace`), without
    /// needing the fold machinery itself: focusing `ns` directly is enough
    /// to exercise `open_file`/`open_diff`'s file-less guard.
    fn state_with_namespace_focus(focus: &str) -> TuiState {
        use crate::graph::model::{FileRef, GitStatus, ModuleNode};
        use std::path::PathBuf as StdPathBuf;

        let ns = crate::graph::model::NodeId::from("ns");
        let leaf_id = crate::graph::model::NodeId::from("leaf");
        let mut nodes = HashMap::new();
        nodes.insert(
            ns.clone(),
            ModuleNode {
                id: ns.clone(),
                display_name: "ns".to_string(),
                parent: None,
                children: vec![leaf_id.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(
            leaf_id.clone(),
            ModuleNode {
                id: leaf_id.clone(),
                display_name: "leaf".to_string(),
                parent: Some(ns.clone()),
                children: vec![],
                status: GitStatus::Modified,
                files: vec![FileRef {
                    path: StdPathBuf::from("leaf.rs"),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        );

        let mut state = state_fixture();
        state.app.graph = ProjectGraph {
            roots: vec![ns, leaf_id],
            nodes,
            edges: vec![],
        };
        state.app.focus = crate::graph::model::NodeId::from(focus);
        state
    }

    #[test]
    fn enter_on_a_file_less_row_sets_a_notice_instead_of_opening_the_file_pane() {
        let mut state = state_with_namespace_focus("ns");
        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(action, KeyAction::Continue));
        assert_eq!(state.app.pane, Pane::Graph, "pane must not flip");
        assert!(
            state.notice.as_deref() == Some(FILE_LESS_ROW_NOTICE),
            "expected the file-less-row notice, got {:?}",
            state.notice
        );
    }

    #[test]
    fn d_on_a_file_less_row_sets_a_notice_instead_of_opening_the_diff_screen() {
        let mut state = state_with_namespace_focus("ns");
        let action = handle_key(&mut state, press('d'));
        assert!(matches!(action, KeyAction::Continue));
        assert_eq!(state.app.screen, Screen::Graph, "screen must not switch");
        assert!(state.notice.as_deref() == Some(FILE_LESS_ROW_NOTICE));
    }

    #[test]
    fn enter_on_an_ordinary_row_opens_the_file_pane_with_no_notice() {
        let mut state = state_with_namespace_focus("leaf");
        let action = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(action, KeyAction::Continue));
        assert_eq!(state.app.pane, Pane::File);
        assert!(state.notice.is_none());
    }
}
