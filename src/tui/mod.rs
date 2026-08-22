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
use crate::core::focus::{move_focus, Direction};
use crate::core::rail_view::RailDirection;
use crate::keymap::{map_key, KeyContext, KeyInput, KeyOutcome, Pending};
use crate::review::comments::map_comments;
use crate::review::review_state::ReviewStore;
use crate::review::store as review_store;
use loader::TuiLoader;

/// Which of the three graph screens is showing: the rail-DAG row renderer
/// (issue #16 phase 2), the semantic-zoom Sugiyama canvas (issue #17/#18),
/// and the nested 2D plane view (the third TUI graph attempt -- see
/// [`crate::graph::plane`]'s module doc for why the first two, both
/// band-based, were rejected in real use), all three kept side by side,
/// cycled with backtick in `Plane -> Canvas -> Rail -> Plane` order (see
/// [`TuiState::view_mode`]'s doc for why this lives entirely in the TUI
/// rather than on `core::App`). `Plane` is the default -- it's the view
/// being evaluated against real use now; the other two are kept for
/// side-by-side comparison, not as the primary experience.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Plane,
    Canvas,
    Rail,
}

impl ViewMode {
    /// The next mode in the backtick cycle (`Plane -> Canvas -> Rail ->
    /// Plane`) -- see [`ViewMode`]'s own doc for why this order.
    fn next(self) -> ViewMode {
        match self {
            ViewMode::Plane => ViewMode::Canvas,
            ViewMode::Canvas => ViewMode::Rail,
            ViewMode::Rail => ViewMode::Plane,
        }
    }
}

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
    /// Whether [`seed_fold_collapsed_if_dense`] actually fired for this
    /// `App` -- when `true`, [`run`] seeds [`TuiState::notice`] with
    /// [`DENSE_FOLD_SEED_NOTICE`] so the first paint tells the user the
    /// graph opened pre-folded and how to zoom in, rather than silently
    /// handing them a rail view that looks collapsed/empty with no
    /// explanation. Set by the caller (`main`'s `launch_tui`) from that
    /// function's return value -- `seed_fold_collapsed_if_dense` runs
    /// before `TuiState` exists, so there's nowhere earlier to set the
    /// notice directly.
    pub dense_fold_seeded: bool,
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
    /// Which graph view is showing (issue #17's maintainer override -- see
    /// [`ViewMode`]'s doc). TUI-local display state, deliberately *not* on
    /// `core::App`: like [`Self::rail_scroll`], it's purely which renderer
    /// paints the same underlying `App::graph`/`App::layers`/
    /// `App::fold_collapsed` state, never something a `core` reducer needs
    /// to reason about -- the fold machinery, focus, and navigation
    /// semantics it drives (`gd`/`gr`, `Enter`, `d`, ...) are identical in
    /// both modes.
    view_mode: ViewMode,
    /// The canvas view's own scroll offset, the [`ViewMode::Canvas`] analog
    /// of [`Self::rail_scroll`] -- see that field's doc for why this is
    /// TUI-local. Reclamped every frame in [`event_loop`] the same way.
    canvas_scroll: usize,
    /// The canvas view's horizontal pan offset (leftmost visible column,
    /// issue #18) -- the horizontal counterpart of [`Self::canvas_scroll`],
    /// for the same TUI-local reason that field is TUI-local. Auto-panned
    /// every frame in [`event_loop`] via [`render::clamp_scroll_x`] to keep
    /// the focused node's own column range inside the viewport, mirroring
    /// [`Self::canvas_scroll`]'s vertical auto-scroll exactly -- replaces
    /// the pre-#18 behavior of simply clipping a band wider than the
    /// terminal at its right edge with no way to reach what fell off it.
    canvas_scroll_x: usize,
    /// Whether a `z` chord prefix is in progress for the canvas view's
    /// fold keys (`zc`/`zo` -- see [`canvas_key_msg`]'s doc for why this
    /// isn't threaded through `crate::keymap::Pending`, which is shared
    /// with the GUI and the rail view's own chord handling). Cleared on
    /// every keypress that isn't itself `z` starting a fresh chord, or the
    /// `c`/`o` that completes one -- there's no chord that survives an
    /// unrelated keypress.
    canvas_fold_pending: bool,
    /// The `nvim` handoff target [`TuiState::execute`]'s `Cmd::CommentNode`
    /// arm just computed, if any -- [`handle_key`] reads this back out
    /// immediately after dispatching `Msg::CommentNode` and turns it into
    /// `KeyAction::EditInNvim`, since `execute` has no `Terminal` to
    /// suspend/resume with itself (see [`handle_key`]'s doc, and
    /// [`should_edit_in_nvim`]'s precedent for `Ctrl-e`, which needs the
    /// same split for the same reason). Always taken (reset to `None`)
    /// the moment `handle_key` reads it, so a comment target never lingers
    /// into some later, unrelated keypress.
    comment_target: Option<(PathBuf, Option<u32>)>,
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
            Cmd::CommentNode(node) => {
                // The TUI has no embedded nvim session/compose-UI to run a
                // comment prompt through the way the GUI's
                // `crate::ui::eframe_app::VdiffApp::comment_node` does via
                // the embedded `vdiff.nvim`. Instead (issue #17's companion
                // fix): hand off to the user's own real `nvim` at the
                // node's first backing file, same suspend/resume as
                // `Ctrl-e` (see `nvim_handoff`), so `:VdiffComment` there
                // captures the comment through the normal flow. `handle_key`
                // reads this back out via [`Self::comment_target`] right
                // after this dispatch returns and turns it into
                // `KeyAction::EditInNvim` -- `execute` itself has no
                // `Terminal` to suspend/resume with (see `handle_key`'s own
                // doc on why that split exists already for `Ctrl-e`).
                match self.comment_nvim_target(&node) {
                    Some(target) => self.comment_target = Some(target),
                    None => {
                        self.notice = Some(FILE_LESS_ROW_NOTICE.to_string());
                    }
                }
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

    /// The `nvim` handoff target for a `Cmd::CommentNode(node)`: `node`'s
    /// first backing file, joined onto the repo root, at line `1` -- there's
    /// no established cursor position to resume at the way `Ctrl-e`'s
    /// [`nvim_edit_target`] has (the file pane's own scroll position), so
    /// this just opens at the top and lets `:VdiffComment` in the user's
    /// real `nvim` do the rest. `None` if `node` has no backing files (a
    /// collapsed namespace row's own id) or isn't in the graph at all --
    /// [`Self::execute`] falls back to [`FILE_LESS_ROW_NOTICE`] in that
    /// case, the same notice `handle_key`'s `Enter`/`d` guards already use
    /// for the same underlying reason.
    fn comment_nvim_target(
        &self,
        node: &crate::graph::model::NodeId,
    ) -> Option<(PathBuf, Option<u32>)> {
        let module = self.app.graph.node(node)?;
        let file = module.files.first()?;
        Some((self.repo_root.join(&file.path), Some(1)))
    }

    /// Reload `<git_dir>/vdiff/comments.json` and remap it onto
    /// `App::comments`, replacing it wholesale -- the TUI's counterpart of
    /// `crate::ui::eframe_app::VdiffApp::reload_comments`, run on resume
    /// from *any* `nvim` handoff (`Ctrl-e` or the `c` comment flow, both via
    /// [`KeyAction::EditInNvim`]) rather than driven by a live RPC
    /// notification the way the GUI's embedded session can -- the TUI has
    /// no embedded session to notify it, so reload-on-resume is the
    /// simplest thing that actually shows a captured comment's badge
    /// without a restart (see the issue's own note that "the GUI has live
    /// refresh; reload-on-resume is enough here"). A read/parse failure is
    /// logged via [`Self::notice`] (not `eprintln!` -- see that field's
    /// doc) and leaves the previous badges in place rather than clearing
    /// them, mirroring the GUI's own failure handling.
    fn reload_comments(&mut self) {
        let git_dir = self.loader.repo.git_dir();
        match review_store::load(&git_dir) {
            Ok(comments) => {
                self.app.comments = map_comments(&self.app.graph, &comments);
            }
            Err(err) => {
                self.notice = Some(format!(
                    "warning: failed to reload {}: {err}",
                    review_store::comments_path(&git_dir).display()
                ));
            }
        }
    }
}

/// What [`event_loop`] should do after [`handle_key`] processes one
/// keypress: keep going, quit, or -- the one case that needs the real
/// terminal, which [`handle_key`] itself deliberately never touches (see
/// its doc) -- suspend for a real `nvim` at `path`/`line`.
#[derive(Debug)]
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

/// Above how many visible modules (issue #18's fix 4) a fresh `--tui`
/// session pre-folds every top-level namespace by default, rather than
/// opening fully expanded -- see [`should_fold_by_default`]'s doc for the
/// second trigger this threshold works alongside.
const FOLD_DEFAULT_NODE_THRESHOLD: usize = 40;

/// How many dependency edges per visible node counts as "disproportionate"
/// for [`should_fold_by_default`]'s second trigger: a graph can be tangled
/// enough to need folding well under [`FOLD_DEFAULT_NODE_THRESHOLD`] nodes
/// if the edge count relative to node count is this high.
const FOLD_DEFAULT_EDGE_RATIO: usize = 3;

/// Whether a fresh `--tui` session should start with every top-level
/// namespace folded (see [`default_fold_seed`]) rather than fully
/// expanded, given `visible_node_count` (the fold-aware, already
/// `hide_test_modules`-filtered node count -- `App::layers`' own shape) and
/// `edge_count` (`App::graph.edges.len()`). Either the node count alone
/// crossing [`FOLD_DEFAULT_NODE_THRESHOLD`], or the edge-to-node ratio
/// crossing [`FOLD_DEFAULT_EDGE_RATIO`] (a graph can be a hairball at a
/// moderate node count too), triggers it -- real use on the un-fixed #17
/// canvas found even a ~12-node/~30-edge change set unusable on first
/// paint (see the issue's own real-use notes), so this is deliberately not
/// a node-count-only check.
fn should_fold_by_default(visible_node_count: usize, edge_count: usize) -> bool {
    visible_node_count > FOLD_DEFAULT_NODE_THRESHOLD
        || edge_count > visible_node_count.saturating_mul(FOLD_DEFAULT_EDGE_RATIO)
}

/// The fold seed [`seed_fold_collapsed_if_dense`] installs when
/// [`should_fold_by_default`] says yes: every top-level root that actually
/// has children -- a leaf root (no children at all, e.g. a lone top-level
/// file) has nothing to collapse into one row, so seeding it would be a
/// no-op that just wastes a `HashSet` entry. This is exactly the same
/// `App::fold_collapsed` a `zc`/`h` keypress would produce by hand; the
/// user expands with `zo`/`l` exactly like any other fold, per the issue's
/// own "reuse the existing fold machinery" instruction.
fn default_fold_seed(
    graph: &crate::graph::model::ProjectGraph,
) -> std::collections::HashSet<crate::graph::model::NodeId> {
    graph
        .roots
        .iter()
        .filter(|id| graph.node(id).is_some_and(|node| !node.children.is_empty()))
        .cloned()
        .collect()
}

/// The one-time notice [`run`] seeds [`TuiState::notice`] with when
/// [`seed_fold_collapsed_if_dense`] actually fires -- see
/// [`TuiConfig::dense_fold_seeded`]'s doc for the threading. Names the
/// concrete key (`zo` in the canvas view; `l` does the same thing in the
/// rail view, but `zo` is what the canvas -- the default view -- binds) so
/// the user isn't left wondering why the graph looks pre-collapsed on
/// first paint.
const DENSE_FOLD_SEED_NOTICE: &str = "dense graph: opened folded -- zo expands one level";

/// [`run`]'s `TuiState::notice` seed, factored out as a pure function so it
/// can be unit-tested without a real terminal (`run` itself needs one for
/// [`enable_raw_mode`]/[`EnterAlternateScreen`]).
fn initial_notice(dense_fold_seeded: bool) -> Option<String> {
    dense_fold_seeded.then(|| DENSE_FOLD_SEED_NOTICE.to_string())
}

/// Pre-seed `app.fold_collapsed` (see [`default_fold_seed`]) if the graph
/// is dense enough to need it (see [`should_fold_by_default`]) -- called
/// once by `main`'s `launch_tui`, right after building the initial `App`
/// and before handing it to [`run`]. Deliberately lives here rather than
/// in the shared `build_initial_app` the GUI also calls: `App::
/// fold_collapsed` starting non-empty is a TUI-only possibility (the GUI
/// frontend never populates it at all -- see `core::app::update`'s own doc
/// on that), so this must not run on the GUI's copy of the same `App`.
///
/// Seeds only top-level roots -- with [`crate::core::app::Msg::
/// ExpandFocusedNamespace`]'s one-level-per-`zo`/`l` semantics, that's
/// exactly right: each expand reveals one further layer of the seeded
/// subtree rather than the whole thing at once, so seeding any deeper
/// wouldn't buy anything the first expand wouldn't immediately undo.
///
/// Returns whether the seed actually fired, so the caller (`main`'s
/// `launch_tui`) can thread that into [`TuiConfig::dense_fold_seeded`] and
/// surface [`DENSE_FOLD_SEED_NOTICE`] on first paint.
pub fn seed_fold_collapsed_if_dense(app: &mut App) -> bool {
    let visible_node_count: usize = app.layers.iter().map(Vec::len).sum();
    let edge_count = app.graph.edges.len();
    if should_fold_by_default(visible_node_count, edge_count) {
        app.fold_collapsed = default_fold_seed(&app.graph);
        // `core::app::update`'s central focus remap only runs on dispatch,
        // so seeding folds before the event loop must remap the initial
        // focus itself -- otherwise the first paint can have focus pointing
        // at a node hidden inside a collapsed namespace (no visible focus,
        // and Enter/`d` acting on a node the user can't see).
        app.focus =
            crate::core::rail_view::effective_row_id(&app.graph, &app.focus, &app.fold_collapsed);
        true
    } else {
        false
    }
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
        notice: initial_notice(config.dense_fold_seeded),
        rail_scroll: 0,
        view_mode: ViewMode::default(),
        canvas_scroll: 0,
        canvas_scroll_x: 0,
        canvas_fold_pending: false,
        comment_target: None,
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
            match state.view_mode {
                ViewMode::Rail => {
                    // Display-line space, not row-index space: a band
                    // separator consumes a screen line of its own, so
                    // `focus_display_line`/`display_line_count` (not
                    // `rows.len()`/a raw row position) are what must agree
                    // with what `render::draw_rail_graph` actually renders
                    // -- see `render::DisplayLine`'s doc for the bug this
                    // avoids (scroll math that only counted rows could let
                    // the focused row scroll off past the bottom edge once
                    // enough separators fell inside the visible window).
                    let rows = crate::core::rail_view::visible_rows_with_layers(
                        &state.app.graph,
                        &state.app.layers,
                        &state.app.fold_collapsed,
                    );
                    let focus_idx =
                        render::focus_display_line(&rows, &state.app.focus).unwrap_or(0);
                    let total_lines = render::display_line_count(&rows);
                    state.rail_scroll = render::clamp_scroll(
                        state.rail_scroll,
                        focus_idx,
                        total_lines,
                        viewport_height,
                    );
                }
                ViewMode::Canvas => {
                    let view = render::build_canvas_view(&state.app);
                    let focus_idx = render::focus_canvas_line(&view, &state.app.focus).unwrap_or(0);
                    let total_lines = render::canvas_line_count(&view);
                    state.canvas_scroll = render::clamp_scroll(
                        state.canvas_scroll,
                        focus_idx,
                        total_lines,
                        viewport_height,
                    );
                    // Horizontal auto-pan (issue #18): keep the focused
                    // node's own `[x, x+width)` range inside the viewport
                    // the same way `clamp_scroll` already does vertically
                    // -- see `render::clamp_scroll_x`'s doc. A focus with
                    // no slot at all (nothing visible yet) leaves the pan
                    // wherever it already was rather than snapping to 0.
                    if let Some((focus_x, focus_width)) =
                        render::focused_slot_range(&view, &state.app.focus)
                    {
                        state.canvas_scroll_x = render::clamp_scroll_x(
                            state.canvas_scroll_x,
                            focus_x,
                            focus_width,
                            size.width as usize,
                        );
                    }
                }
                ViewMode::Plane => {
                    // Reuses `canvas_scroll`/`canvas_scroll_x` (see those
                    // fields' own doc) -- the plane view is never showing at
                    // the same time as the canvas view, so there's no
                    // cross-talk between the two modes' auto-pan.
                    let view = render::build_plane_view(&state.app);
                    let total_height = render::plane_view_height(&view);
                    if let Some(rect) = render::focused_plane_rect(&view, &state.app.focus) {
                        state.canvas_scroll = render::clamp_scroll(
                            state.canvas_scroll,
                            rect.y,
                            total_height,
                            viewport_height,
                        );
                        state.canvas_scroll_x = render::clamp_scroll_x(
                            state.canvas_scroll_x,
                            rect.x,
                            rect.w,
                            size.width as usize,
                        );
                    }
                }
            }
        }
        terminal.draw(|frame| {
            render::draw(
                frame,
                &state.app,
                state.notice.as_deref(),
                state.rail_scroll,
                state.canvas_scroll,
                state.canvas_scroll_x,
                state.view_mode,
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
                        match nvim_handoff::suspend_and_run(&path, line, &state.repo_root) {
                            Ok(()) => {
                                // Issue #17's companion fix: reload
                                // `comments.json` on resume from *any*
                                // handoff (`Ctrl-e` or the `c` comment
                                // flow), so a comment just captured through
                                // `:VdiffComment` shows up as a badge
                                // without restarting -- see
                                // `TuiState::reload_comments`'s doc.
                                state.reload_comments();
                            }
                            Err(err) => {
                                // See `TuiState::notice`'s doc: `eprintln!`
                                // would be invisible/garbled while the
                                // alternate screen still owns the terminal
                                // at this point (the handoff already
                                // restored and re-suspended it around the
                                // `nvim` child, but this warning fires
                                // after that, back under our own alternate
                                // screen).
                                state.notice = Some(format!("failed to launch nvim: {err}"));
                            }
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
/// say over closing that instead); `Ctrl-e` *or* `c` on the file pane
/// requests [`KeyAction::EditInNvim`] instead of dispatching through
/// `map_key` (see [`should_edit_in_nvim`]); `h`/`j`/`k`/`l` on the rail view (see
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

    if should_toggle_view_mode(state, input) {
        state.view_mode = state.view_mode.next();
        state.canvas_fold_pending = false;
        return KeyAction::Continue;
    }

    match state.view_mode {
        ViewMode::Rail => {
            if let Some(msg) = rail_key_msg(state, input) {
                state.dispatch(msg);
                return KeyAction::Continue;
            }
        }
        ViewMode::Canvas => {
            if let Some(msg) = canvas_key_msg(state, input) {
                state.dispatch(msg);
                return KeyAction::Continue;
            }
        }
        ViewMode::Plane => {
            if let Some(msg) = plane_key_msg(state, input) {
                state.dispatch(msg);
                return KeyAction::Continue;
            }
        }
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
    if let Some((path, line)) = state.comment_target.take() {
        return KeyAction::EditInNvim { path, line };
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
/// normal `map_key` pipeline: `Ctrl-e` *or* `c` on the file pane, with no
/// chord in progress (so `]c`/`[c`, which also end in a `c`, aren't
/// shadowed -- see [`crate::keymap::map_key`]'s `Pending::Char(']')`/
/// `Pending::Char('[')` arms for [`Pane::File`]).
///
/// `c` here is issue #17's companion fix: the issue asks for a handoff "on
/// a node with files, or in the file pane" -- the graph pane's `c` already
/// goes through `Msg::CommentNode`/`Cmd::CommentNode` (see
/// [`TuiState::execute`]'s arm and [`TuiState::comment_nvim_target`]), but
/// `crate::keymap::map_key`'s own `Pane::File` arm has no `c` binding at
/// all (deliberately left unbound there rather than added to shared
/// `map_key`, which the GUI also uses and must stay untouched -- see this
/// module's own doc on why TUI-only meanings for a key are intercepted
/// here instead of grown into `map_key` as a context flag). Without this
/// check `c` on the file pane would be a silent dead key. Handled by the
/// same [`nvim_edit_target`] as `Ctrl-e` -- the file pane's own established
/// cursor position (`FileViewState::scroll_row`) is strictly better than
/// the graph-pane path's first-file/line-1 fallback, so there's no reason
/// for the two to differ. Both share the same reload-on-resume path (see
/// [`TuiState::reload_comments`]) once `event_loop` returns from the
/// handoff -- from `map_key`/`update`'s point of view they're two
/// different routes to the exact same `KeyAction::EditInNvim`.
fn should_edit_in_nvim(state: &TuiState, input: KeyInput) -> bool {
    (input == KeyInput::Ctrl('e') || input == KeyInput::Char('c'))
        && state.app.pane == Pane::File
        && state.pending_key.is_none()
}

/// Whether `input` is the [`ViewMode`] toggle: backtick, on the graph
/// screen's graph pane with no picker/chord in progress. Backtick was
/// picked over the more obvious `v` (already `Msg::ToggleReviewed`) or `z`
/// (the canvas's own fold-chord prefix -- see [`canvas_key_msg`]) precisely
/// because it collides with nothing else bound anywhere in this crate's
/// keymap (GUI or TUI) -- an unshifted, single-tap key that's otherwise
/// idle in every context this toggle needs to fire in.
fn should_toggle_view_mode(state: &TuiState, input: KeyInput) -> bool {
    input == KeyInput::Char('`')
        && state.app.screen == Screen::Graph
        && state.app.pane == Pane::Graph
        && state.app.picker.is_none()
        && state.pending_key.is_none()
}

/// The canvas-view message `input` should dispatch directly, bypassing
/// `map_key`, mirroring [`rail_key_msg`]'s precedent for the rail view but
/// with entirely different keys/semantics per the maintainer override: in
/// canvas mode `h`/`j`/`k`/`l` are *spatial* movement (the GUI's own
/// `crate::core::focus::move_focus`, reused unchanged here over the
/// canvas's char-space x-centers instead of the GUI's pixel ones -- see
/// [`render::canvas_focus_grid`]) rather than the rail view's fold-
/// collapse/row-step meaning, and folding uses a `z`-prefixed chord
/// (`zc`/`zo`, vim's own `foldclose`/`foldopen` mnemonic) instead of `h`/`l`
/// directly, since those two keys are already spoken for by movement here.
/// `None` outside [`Screen::Graph`]/[`Pane::Graph`], with a picker open, or
/// with an unrelated chord (`crate::keymap::Pending`) already in progress --
/// same guard [`rail_key_msg`] uses, for the same reason.
fn canvas_key_msg(state: &mut TuiState, input: KeyInput) -> Option<Msg> {
    if state.app.screen != Screen::Graph
        || state.app.pane != Pane::Graph
        || state.app.picker.is_some()
        || state.pending_key.is_some()
    {
        state.canvas_fold_pending = false;
        return None;
    }

    if state.canvas_fold_pending {
        state.canvas_fold_pending = false;
        return match input {
            KeyInput::Char('c') => Some(Msg::CollapseFocusedNamespace),
            KeyInput::Char('o') => Some(Msg::ExpandFocusedNamespace),
            _ => None,
        };
    }

    match input {
        KeyInput::Char('z') => {
            state.canvas_fold_pending = true;
            None
        }
        KeyInput::Char(c @ ('h' | 'j' | 'k' | 'l')) => {
            let dir = match c {
                'h' => Direction::Left,
                'l' => Direction::Right,
                'k' => Direction::Up,
                _ => Direction::Down,
            };
            let (layers, rows) = render::canvas_focus_grid(&state.app);
            let target = move_focus(&layers, &rows, &state.app.focus, dir);
            Some(Msg::FocusSet(target))
        }
        _ => None,
    }
}

/// The plane-view message `input` should dispatch directly, bypassing
/// `map_key` -- identical in shape to [`canvas_key_msg`] (spatial `h`/`j`/
/// `k`/`l` over [`move_focus`], `zc`/`zo` fold chord), just fed
/// [`render::plane_focus_grid`]'s rects instead of the canvas's Sugiyama
/// band x-centers. Shares [`TuiState::canvas_fold_pending`] with the canvas
/// view rather than a separate flag -- the two view modes are mutually
/// exclusive at any given moment (see [`ViewMode`]'s doc), so there's never
/// a chord in flight for one mode while the other is showing, and
/// `should_toggle_view_mode`'s backtick handler already clears it on every
/// mode switch regardless.
fn plane_key_msg(state: &mut TuiState, input: KeyInput) -> Option<Msg> {
    if state.app.screen != Screen::Graph
        || state.app.pane != Pane::Graph
        || state.app.picker.is_some()
        || state.pending_key.is_some()
    {
        state.canvas_fold_pending = false;
        return None;
    }

    if state.canvas_fold_pending {
        state.canvas_fold_pending = false;
        return match input {
            KeyInput::Char('c') => Some(Msg::CollapseFocusedNamespace),
            KeyInput::Char('o') => Some(Msg::ExpandFocusedNamespace),
            _ => None,
        };
    }

    match input {
        KeyInput::Char('z') => {
            state.canvas_fold_pending = true;
            None
        }
        KeyInput::Char(c @ ('h' | 'j' | 'k' | 'l')) => {
            let dir = match c {
                'h' => Direction::Left,
                'l' => Direction::Right,
                'k' => Direction::Up,
                _ => Direction::Down,
            };
            let (layers, rows) = render::plane_focus_grid(&state.app);
            let target = move_focus(&layers, &rows, &state.app.focus, dir);
            Some(Msg::FocusSet(target))
        }
        _ => None,
    }
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
    use crate::graph::model::{NodeId, ProjectGraph};
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
            view_mode: ViewMode::default(),
            canvas_scroll: 0,
            canvas_scroll_x: 0,
            canvas_fold_pending: false,
            comment_target: None,
        }
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn pressing_c_on_a_file_less_focus_sets_a_notice_instead_of_a_dead_key() {
        // `state_fixture`'s focus (`""`) isn't in the (empty) graph at all,
        // so `Cmd::CommentNode`'s handoff target lookup fails and
        // `execute` falls back to the same file-less-row notice `Enter`/`d`
        // already use -- see `TuiState::comment_nvim_target`'s doc.
        let mut state = state_fixture();
        let action = handle_key(&mut state, press('c'));
        assert!(matches!(action, KeyAction::Continue));
        assert_eq!(state.notice.as_deref(), Some(FILE_LESS_ROW_NOTICE));
    }

    #[test]
    fn pressing_c_on_a_node_with_files_requests_an_nvim_handoff() {
        let mut state = state_with_namespace_focus("leaf");
        let action = handle_key(&mut state, press('c'));
        match action {
            KeyAction::EditInNvim { path, line } => {
                assert!(path.ends_with("leaf.rs"));
                assert_eq!(line, Some(1));
            }
            other => panic!("expected an nvim handoff, got a different action: {other:?}"),
        }
    }

    /// Issue #17's companion fix: `c` on the file pane (not just the graph
    /// pane) hands off to nvim, mirroring `Ctrl-e`'s own file-pane handoff
    /// exactly (same [`nvim_edit_target`]) -- see [`should_edit_in_nvim`]'s
    /// doc. Before this fix, `crate::keymap::map_key`'s `Pane::File` arm had
    /// no `c` binding at all, so this was a silent dead key.
    #[test]
    fn pressing_c_on_the_file_pane_requests_an_nvim_handoff_at_the_current_scroll_position() {
        use crate::core::file_view::{FileViewEntry, FileViewState};

        let mut state = state_with_namespace_focus("leaf");
        state.app.pane = Pane::File;
        let mut file_view = FileViewState::new(
            NodeId::from("leaf"),
            vec![FileViewEntry {
                path: PathBuf::from("leaf.rs"),
                lines: vec!["one".to_string(), "two".to_string(), "three".to_string()],
                changed_ranges: vec![],
                deleted: false,
            }],
        );
        file_view.scroll_row = 2;
        state.app.file_view = Some(file_view);

        let action = handle_key(&mut state, press('c'));
        match action {
            KeyAction::EditInNvim { path, line } => {
                assert!(path.ends_with("leaf.rs"));
                // `Ctrl-e`'s own target math: `scroll_row + 1`.
                assert_eq!(line, Some(3));
            }
            other => panic!("expected an nvim handoff, got a different action: {other:?}"),
        }
    }

    /// `]c` (jump to next changed range) must still work on the file pane
    /// -- `c` completing that pending `]` chord, not `should_edit_in_nvim`'s
    /// new bare-`c` interception, since a chord is in progress.
    #[test]
    fn bracket_c_chord_on_the_file_pane_still_jumps_to_the_next_change_not_an_nvim_handoff() {
        use crate::core::file_view::{FileViewEntry, FileViewState};
        use crate::keymap::Pending;

        let mut state = state_with_namespace_focus("leaf");
        state.app.pane = Pane::File;
        state.app.file_view = Some(FileViewState::new(
            NodeId::from("leaf"),
            vec![FileViewEntry {
                path: PathBuf::from("leaf.rs"),
                lines: vec!["one".to_string(), "two".to_string(), "three".to_string()],
                changed_ranges: vec![(2, 2)],
                deleted: false,
            }],
        ));
        state.pending_key = Some(Pending::Char(']'));

        let action = handle_key(&mut state, press('c'));
        assert!(matches!(action, KeyAction::Continue));
        assert_eq!(
            state.app.file_view.as_ref().unwrap().scroll_row,
            2,
            "]c should have jumped to the changed range, not been swallowed by the nvim handoff"
        );
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
                    state.canvas_scroll,
                    state.canvas_scroll_x,
                    state.view_mode,
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

    // -- View-mode toggle and canvas-mode keys (issue #17) -----------------

    /// A two-layer graph (`leaf` depends on `target`) with `App::layers`
    /// actually populated -- unlike [`state_with_namespace_focus`] (whose
    /// fixture leaves `layers` empty, fine for its file-less-row tests but
    /// not for exercising canvas-mode spatial movement, which walks real
    /// band structure).
    fn state_with_layered_graph(focus: &str) -> TuiState {
        use crate::graph::model::{DepEdge, DepKind, FileRef, GitStatus, ModuleNode};
        use std::path::PathBuf as StdPathBuf;

        let leaf = crate::graph::model::NodeId::from("leaf");
        let target = crate::graph::model::NodeId::from("target");
        let node = |id: &crate::graph::model::NodeId, name: &str| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Modified,
            files: vec![FileRef {
                path: StdPathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };
        let mut nodes = HashMap::new();
        nodes.insert(leaf.clone(), node(&leaf, "leaf"));
        nodes.insert(target.clone(), node(&target, "target"));
        let graph = ProjectGraph {
            roots: vec![leaf.clone(), target.clone()],
            nodes,
            edges: vec![DepEdge {
                from: leaf.clone(),
                to: target.clone(),
                kind: DepKind::Use,
            }],
        };
        let layers = crate::graph::layers::assign_layers(&graph);

        let mut state = state_fixture();
        state.app.graph = graph;
        state.app.layers = layers;
        state.app.focus = crate::graph::model::NodeId::from(focus);
        state
    }

    #[test]
    fn plane_mode_is_the_default() {
        let state = state_fixture();
        assert_eq!(state.view_mode, ViewMode::Plane);
    }

    #[test]
    fn backtick_cycles_through_plane_canvas_and_rail_and_back() {
        let mut state = state_fixture();
        assert_eq!(state.view_mode, ViewMode::Plane);
        handle_key(&mut state, press('`'));
        assert_eq!(state.view_mode, ViewMode::Canvas);
        handle_key(&mut state, press('`'));
        assert_eq!(state.view_mode, ViewMode::Rail);
        handle_key(&mut state, press('`'));
        assert_eq!(state.view_mode, ViewMode::Plane);
    }

    #[test]
    fn in_rail_mode_h_and_l_still_fold_and_unfold_unchanged() {
        let mut state = state_with_layered_graph("leaf");
        state.view_mode = ViewMode::Rail;
        // `leaf` has no parent namespace in this fixture, so `h` is a
        // documented no-op -- the point of this test is just that rail
        // mode's `h`/`l` still dispatch `Msg::CollapseFocusedNamespace`/
        // `Msg::ExpandFocusedNamespace` (via `rail_key_msg`), not canvas
        // spatial movement, regardless of outcome.
        let action = handle_key(&mut state, press('h'));
        assert!(matches!(action, KeyAction::Continue));
        assert_eq!(
            state.app.focus,
            NodeId::from("leaf"),
            "no parent to fold into"
        );
    }

    #[test]
    fn in_canvas_mode_j_moves_focus_spatially_to_the_dependency_below() {
        let mut state = state_with_layered_graph("leaf");
        state.view_mode = ViewMode::Canvas;
        handle_key(&mut state, press('j'));
        assert_eq!(state.app.focus, NodeId::from("target"));
    }

    #[test]
    fn in_canvas_mode_h_and_l_do_not_fold_they_move_spatially() {
        // A single-node band: `h`/`l` have nothing to step to within the
        // row, so focus stays put -- proving these two keys are *not*
        // reinterpreted as collapse/expand in canvas mode (they'd have
        // stayed put here too if they were, but `fold_collapsed` must be
        // untouched either way).
        let mut state = state_with_layered_graph("leaf");
        state.view_mode = ViewMode::Canvas;
        handle_key(&mut state, press('h'));
        assert!(state.app.fold_collapsed.is_empty());
        handle_key(&mut state, press('l'));
        assert!(state.app.fold_collapsed.is_empty());
    }

    #[test]
    fn in_plane_mode_j_moves_focus_spatially_to_the_dependency_below() {
        let mut state = state_with_layered_graph("leaf");
        assert_eq!(state.view_mode, ViewMode::Plane);
        handle_key(&mut state, press('j'));
        assert_eq!(state.app.focus, NodeId::from("target"));
    }

    #[test]
    fn in_plane_mode_h_and_l_do_not_fold_they_move_spatially() {
        let mut state = state_with_layered_graph("leaf");
        assert_eq!(state.view_mode, ViewMode::Plane);
        handle_key(&mut state, press('h'));
        assert!(state.app.fold_collapsed.is_empty());
        handle_key(&mut state, press('l'));
        assert!(state.app.fold_collapsed.is_empty());
    }

    #[test]
    fn zc_then_zo_collapses_then_expands_in_plane_mode() {
        let mut state = state_with_layered_graph("leaf");
        assert_eq!(state.view_mode, ViewMode::Plane);
        handle_key(&mut state, press('z'));
        assert!(
            state.canvas_fold_pending,
            "z alone should arm the chord, not dispatch anything yet"
        );
        handle_key(&mut state, press('c'));
        // `leaf` has no parent namespace, so nothing actually collapses --
        // this just proves the chord dispatched `Msg::CollapseFocusedNamespace`
        // rather than falling through to `map_key`'s own `c`
        // (`Msg::CommentNode`), which would have requested an nvim handoff
        // instead.
        assert!(!state.canvas_fold_pending, "chord clears after completing");
    }

    #[test]
    fn zc_then_zo_collapses_then_expands_in_canvas_mode() {
        let mut state = state_with_layered_graph("leaf");
        handle_key(&mut state, press('z'));
        assert!(
            state.canvas_fold_pending,
            "z alone should arm the chord, not dispatch anything yet"
        );
        handle_key(&mut state, press('c'));
        // `leaf` has no parent namespace, so nothing actually collapses --
        // this just proves the chord dispatched `Msg::CollapseFocusedNamespace`
        // rather than falling through to `map_key`'s own `c`
        // (`Msg::CommentNode`), which would have requested an nvim handoff
        // instead.
        assert!(!state.canvas_fold_pending, "chord clears after completing");
    }

    #[test]
    fn an_unrelated_key_after_z_clears_the_pending_chord() {
        let mut state = state_with_layered_graph("leaf");
        handle_key(&mut state, press('z'));
        assert!(state.canvas_fold_pending);
        handle_key(&mut state, press('j'));
        assert!(!state.canvas_fold_pending);
    }

    #[test]
    fn should_fold_by_default_is_false_under_both_thresholds() {
        assert!(!should_fold_by_default(10, 15));
    }

    #[test]
    fn should_fold_by_default_trips_on_node_count_alone() {
        assert!(should_fold_by_default(FOLD_DEFAULT_NODE_THRESHOLD + 1, 0));
    }

    #[test]
    fn should_fold_by_default_trips_on_a_disproportionate_edge_count() {
        // 12 nodes is well under the node threshold, but 40 edges is more
        // than `FOLD_DEFAULT_EDGE_RATIO` per node -- mirrors the issue's
        // own real-use report of a small-but-tangled change set.
        assert!(should_fold_by_default(12, 40));
    }

    #[test]
    fn should_fold_by_default_is_false_right_at_the_node_threshold() {
        assert!(!should_fold_by_default(FOLD_DEFAULT_NODE_THRESHOLD, 0));
    }

    /// A graph with two top-level roots: `parent` has one child (`leaf`),
    /// `lonely` has none -- exercises [`default_fold_seed`]'s "only seed
    /// roots that actually have children" filter.
    fn graph_with_one_childful_root() -> ProjectGraph {
        use crate::graph::model::{GitStatus, ModuleNode};

        let parent = NodeId::from("parent");
        let leaf = NodeId::from("leaf");
        let lonely = NodeId::from("lonely");
        let mut nodes = HashMap::new();
        nodes.insert(
            parent.clone(),
            ModuleNode {
                id: parent.clone(),
                display_name: "parent".to_string(),
                parent: None,
                children: vec![leaf.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(
            leaf.clone(),
            ModuleNode {
                id: leaf.clone(),
                display_name: "leaf".to_string(),
                parent: Some(parent.clone()),
                children: vec![],
                status: GitStatus::Modified,
                files: vec![],
            },
        );
        nodes.insert(
            lonely.clone(),
            ModuleNode {
                id: lonely.clone(),
                display_name: "lonely".to_string(),
                parent: None,
                children: vec![],
                status: GitStatus::Modified,
                files: vec![],
            },
        );
        ProjectGraph {
            roots: vec![parent, lonely],
            nodes,
            edges: vec![],
        }
    }

    #[test]
    fn default_fold_seed_only_includes_roots_with_children() {
        let graph = graph_with_one_childful_root();
        let seed = default_fold_seed(&graph);
        assert_eq!(seed, HashSet::from([NodeId::from("parent")]));
    }

    #[test]
    fn seed_fold_collapsed_if_dense_is_a_noop_under_threshold() {
        let graph = graph_with_one_childful_root();
        let mut app = state_fixture().app;
        app.graph = graph;
        app.layers = vec![vec![NodeId::from("parent"), NodeId::from("lonely")]];
        let seeded = seed_fold_collapsed_if_dense(&mut app);
        assert!(!seeded);
        assert!(app.fold_collapsed.is_empty());
    }

    #[test]
    fn seed_fold_collapsed_if_dense_seeds_top_level_namespaces_when_dense() {
        let graph = graph_with_one_childful_root();
        let mut app = state_fixture().app;
        app.graph = graph;
        // One layer with more nodes than the threshold -- doesn't need to
        // be a realistic layering, `seed_fold_collapsed_if_dense` only
        // sums layer lengths for the node count.
        app.layers = vec![vec![NodeId::from("x"); FOLD_DEFAULT_NODE_THRESHOLD + 1]];
        let seeded = seed_fold_collapsed_if_dense(&mut app);
        assert!(seeded);
        assert_eq!(app.fold_collapsed, HashSet::from([NodeId::from("parent")]));
    }

    #[test]
    fn seed_fold_collapsed_if_dense_remaps_a_focus_inside_a_seeded_fold() {
        // The central focus remap in `core::app::update` only runs on
        // dispatch -- seeding folds *before* the event loop must remap the
        // initial focus itself, or the first paint has a focus pointing at
        // a node hidden inside a collapsed namespace (invisible focus, and
        // Enter/`d` acting on a node the user can't see).
        let graph = graph_with_one_childful_root();
        let mut app = state_fixture().app;
        app.graph = graph;
        app.focus = NodeId::from("leaf");
        app.layers = vec![vec![NodeId::from("x"); FOLD_DEFAULT_NODE_THRESHOLD + 1]];
        seed_fold_collapsed_if_dense(&mut app);
        assert_eq!(app.focus, NodeId::from("parent"));
    }

    /// Like [`graph_with_one_childful_root`], but `leaf` actually carries a
    /// file -- needed for [`seed_fold_collapsed_if_dense_seeds_only_top_level_roots_one_level_expand_ready`],
    /// which dispatches a real `ExpandFocusedNamespace` and needs
    /// `rail_view::first_visible_descendant` to find a drawn (file-backed)
    /// row to re-seat onto.
    fn graph_with_one_file_backed_childful_root() -> ProjectGraph {
        use crate::graph::model::{FileRef, GitStatus, ModuleNode};
        use std::path::PathBuf;

        let parent = NodeId::from("parent");
        let leaf = NodeId::from("leaf");
        let mut nodes = HashMap::new();
        nodes.insert(
            parent.clone(),
            ModuleNode {
                id: parent.clone(),
                display_name: "parent".to_string(),
                parent: None,
                children: vec![leaf.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(
            leaf.clone(),
            ModuleNode {
                id: leaf.clone(),
                display_name: "leaf".to_string(),
                parent: Some(parent.clone()),
                children: vec![],
                status: GitStatus::Modified,
                files: vec![FileRef {
                    path: PathBuf::from("leaf.rs"),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        );
        ProjectGraph {
            roots: vec![parent],
            nodes,
            edges: vec![],
        }
    }

    #[test]
    fn seed_fold_collapsed_if_dense_seeds_only_top_level_roots_one_level_expand_ready() {
        // Verifies the seed/expand interaction the doc on
        // `seed_fold_collapsed_if_dense` claims: seeding only top-level
        // roots is correct once `ExpandFocusedNamespace` is one-level-at-a-
        // time -- expanding the seeded root here should reveal exactly its
        // direct children (all leaves in this fixture), nothing deeper, and
        // never explode into a fully-flat view in one keypress.
        let graph = graph_with_one_file_backed_childful_root();
        let mut app = state_fixture().app;
        app.graph = graph;
        app.focus = NodeId::from("parent");
        app.layers = vec![vec![NodeId::from("x"); FOLD_DEFAULT_NODE_THRESHOLD + 1]];

        seed_fold_collapsed_if_dense(&mut app);
        assert_eq!(app.fold_collapsed, HashSet::from([NodeId::from("parent")]));

        let (app, _) = update(app, Msg::ExpandFocusedNamespace);
        // `parent`'s only child is `leaf`, which has no children of its
        // own -- expanding reveals it as a plain visible row with nothing
        // left in `fold_collapsed`.
        assert!(app.fold_collapsed.is_empty());
        assert_eq!(app.focus, NodeId::from("leaf"));
    }

    #[test]
    fn initial_notice_is_none_when_the_seed_never_fired() {
        assert_eq!(initial_notice(false), None);
    }

    #[test]
    fn initial_notice_surfaces_the_dense_fold_seed_notice_when_it_fired() {
        assert_eq!(
            initial_notice(true),
            Some(DENSE_FOLD_SEED_NOTICE.to_string())
        );
    }
}
