//! [`eframe::App`] glue: owns [`core::App`] plus view-only state (the pan/
//! zoom [`Transform`], the last-followed focus, and the pending `g`-chord
//! char), translates egui key events into [`crate::keymap::KeyInput`],
//! threads them through [`crate::keymap::map_key`] and
//! [`crate::core::app::update`], and executes the resulting [`Cmd`]. All
//! I/O and toolkit-specific state lives here -- `core::App`/`update` stay
//! pure.
//!
//! Keyboard zoom (`+`/`=`/`-`) is the one exception to the `map_key`
//! pipeline: it only ever changes the view-only `Transform`, never
//! `core::App`, so it's handled directly in [`VdiffApp::handle_zoom_keys`]
//! instead of growing `crate::keymap::KeyInput` with a variant the reducer
//! would never act on.
//!
//! The embedded-nvim mode (default-on, `--no-nvim` to opt out; see
//! [`crate::nvim`]) is the other exception: when [`VdiffApp::nvim`] is
//! `Some` and keyboard focus is on
//! [`Pane::File`], raw egui input bypasses `map_key` entirely in favor of
//! [`VdiffApp::handle_nvim_keys`], which delegates the actual event
//! processing to the pure [`crate::ui::nvim_pane::process_nvim_events`] --
//! see that function's doc for why the `Ctrl-w` pane-switch chord in
//! particular needs to be handled against real egui event sequences
//! rather than per-event here. `core::App`/`update` never learn nvim
//! mode exists: [`Cmd::LoadFile`] loads the same real [`FileViewState`]
//! either way (see [`VdiffApp::load_file`]) and, in nvim mode, additionally
//! opens the file in the session via
//! [`crate::nvim::session::NvimCmd::OpenFile`] -- the loaded state still
//! flows through the reducer so `file_view.is_some()`-gated invariants
//! (the file pane is open, `Ctrl-w l` has something to switch to) hold
//! without teaching `core` a second file-viewing mode; it's also where the
//! changed-line-mark ranges [`NvimPane::open_file`] needs come from.

use std::time::{Duration, Instant};

use egui::{Align2, Context, Key, Modifiers};

use crate::core::app::{update, App, Cmd, Msg, Pane, Screen};
use crate::core::diff_state::{DiffPaneState, FileEntry};
use crate::core::file_view::{FileViewEntry, FileViewState};
use crate::core::focus::Direction;
use crate::graph::layout::{layout_with_test_strips, LayoutResult};
use crate::graph::model::{GitStatus, ModuleNode, NodeId, ProjectGraph};
use crate::keymap::{map_key, KeyContext, KeyInput, KeyOutcome, Pending};
use crate::nvim::session::NvimCmd;
use crate::pipeline::file_diff::{changed_head_ranges, load_file_diff};
use crate::pipeline::repo::GitRepo;
use crate::review::comments::map_comments;
use crate::review::review_state::ReviewStore;
use crate::review::store as review_store;
use crate::ui::diff_view;
use crate::ui::graph_view::{self, GraphViewCache, Transform};
use crate::ui::nvim_pane::{self, NvimAction, NvimPane};
use crate::ui::overlay;

/// How long `--smoke` keeps the window open before closing it.
const SMOKE_DURATION: Duration = Duration::from_secs(2);

/// Scale multiplier applied per `+`/`-` keyboard zoom press.
const ZOOM_KEY_FACTOR: f32 = 1.2;

/// Everything [`Cmd::LoadDiff`] needs to read file content from git: the
/// repository and the diff base it was resolved against at startup. Lives
/// alongside [`VdiffApp`] rather than in `core` -- it's I/O, not state.
pub struct DiffLoader {
    pub repo: Box<dyn GitRepo>,
    pub base_oid: String,
}

impl DiffLoader {
    /// Load every file backing `node` in `graph`: base content from
    /// [`GitRepo::base_blob`] at `self.base_oid` (empty for added files),
    /// head content from [`GitRepo::head_content`] (empty for deleted
    /// files), diffed with [`diff_file`]. Errors as a message string,
    /// matching [`Msg::LoadFailed`]'s shape.
    fn load(&self, graph: &ProjectGraph, node: &NodeId) -> Result<DiffPaneState, String> {
        let module = graph
            .node(node)
            .ok_or_else(|| format!("node {node} not found in graph"))?;

        let mut files = Vec::with_capacity(module.files.len());
        for file_ref in &module.files {
            let diff = load_file_diff(self.repo.as_ref(), &self.base_oid, file_ref)?;
            files.push(FileEntry {
                path: file_ref.path.clone(),
                diff,
            });
        }

        Ok(DiffPaneState::new(node.clone(), files))
    }

    /// Load the file-viewer state for every file backing `node` in `graph`:
    /// head content via [`GitRepo::head_content`], or -- for a deleted
    /// file (no `head_blob`) -- base content via [`GitRepo::base_blob`],
    /// flagged [`FileViewEntry::deleted`] so the renderer can say so.
    /// `changed_ranges` come from [`load_file_diff`]'s hunks when the node
    /// has actually changed and the file has content on either side --
    /// deleted files (no head content to mark up) always get an empty
    /// range list. Errors as a message string, matching
    /// [`Msg::LoadFailed`]'s shape.
    fn load_file_view(&self, graph: &ProjectGraph, node: &NodeId) -> Result<FileViewState, String> {
        let module = graph
            .node(node)
            .ok_or_else(|| format!("node {node} not found in graph"))?;

        let mut files = Vec::with_capacity(module.files.len());
        for file_ref in &module.files {
            files.push(self.load_file_view_entry(module, file_ref)?);
        }

        Ok(FileViewState::new(node.clone(), files))
    }

    /// Load one [`FileViewEntry`] -- see [`Self::load_file_view`].
    fn load_file_view_entry(
        &self,
        module: &ModuleNode,
        file_ref: &crate::graph::model::FileRef,
    ) -> Result<FileViewEntry, String> {
        let deleted = file_ref.head_blob.is_none();
        let content = if deleted {
            self.repo
                .base_blob(&self.base_oid, &file_ref.path)
                .map_err(|err| err.to_string())?
                .unwrap_or_default()
        } else {
            self.repo
                .head_content(&file_ref.path)
                .map_err(|err| err.to_string())?
                .unwrap_or_default()
        };
        let lines: Vec<String> = content.lines().map(str::to_string).collect();

        let changed_ranges = if !deleted
            && module.status != GitStatus::Unchanged
            && (file_ref.base_blob.is_some() || file_ref.head_blob.is_some())
        {
            let diff = load_file_diff(self.repo.as_ref(), &self.base_oid, file_ref)?;
            changed_head_ranges(&diff)
        } else {
            Vec::new()
        };

        Ok(FileViewEntry {
            path: file_ref.path.clone(),
            lines,
            changed_ranges,
            deleted,
        })
    }
}

/// Owns [`core::App`] and drives it from egui input/paint each frame.
pub struct VdiffApp {
    app: App,
    layout: LayoutResult,
    transform: Transform,
    /// The focus [`graph_view::show`]'s auto-pan last ran for -- lets it
    /// fire only when focus actually changes rather than every repaint.
    last_focus: Option<NodeId>,
    /// Whether [`Self::show_graph_screen`] has already applied the initial
    /// horizontal-centering pan (see [`graph_view::initial_x_offset`]).
    /// `false` until the graph's first frame, when the viewport size is
    /// first known; flipped to `true` right after, so this runs exactly
    /// once per app lifetime rather than fighting the user's own pan on
    /// every subsequent repaint (same one-shot concern
    /// [`graph_view::show`]'s `last_focus` gating documents for auto-pan).
    initial_view_centered: bool,
    pending_key: Option<Pending>,
    smoke: bool,
    started_at: Instant,
    diff_loader: DiffLoader,
    /// The live embedded-nvim session, if nvim mode is on (the default,
    /// unless `--no-nvim` was given) and an `nvim` binary was found (see
    /// `main.rs`'s startup decision). Once `Some`,
    /// stays `Some` for the lifetime of the app even if the underlying
    /// process dies -- see [`Self::logic`]/[`Self::respawn_nvim`]; a dead
    /// session is replaced in place, never dropped down to `None` (unless
    /// the respawn itself fails to spawn, which is treated as reverting to
    /// the built-in viewer for the rest of the run).
    nvim: Option<NvimPane>,
    /// Whether the previous nvim-mode keypress was `Ctrl-w`, awaiting a
    /// completing key -- the local, nvim-side equivalent of
    /// [`crate::keymap::Pending::CtrlW`] (kept separately rather than
    /// reusing `pending_key`, since nvim-mode input bypasses `map_key`
    /// entirely; see [`Self::handle_nvim_keys`]).
    nvim_ctrl_w_pending: bool,
    /// `nvim --embed`'s working directory, remembered for
    /// [`Self::respawn_nvim`] (the initial spawn happens in `main.rs`,
    /// before a `VdiffApp` exists to remember it itself).
    nvim_cwd: std::path::PathBuf,
    /// Ex commands to run after every `nvim_ui_attach` -- the initial spawn
    /// and every respawn (see `--nvim-cmd` in `main.rs`'s CLI).
    nvim_init_cmds: Vec<String>,
    /// The egui context, kept around so [`Self::respawn_nvim`] can spawn a
    /// fresh [`NvimPane`] (which needs a `Context` to request repaints)
    /// outside of a paint callback.
    egui_ctx: Context,
    /// The path most recently opened in the nvim pane via graph navigation
    /// (`None` before the first `OpenFile` in nvim mode, or whenever nvim
    /// mode is off). Demoted to a fallback: `:VdiffDiff`/`d`'s diffsplit
    /// asks nvim what buffer is *actually* current first (see
    /// [`Self::trigger_vdiff_diff`]/[`crate::nvim::vdiff_glue::resolve_diffed_path`]) --
    /// this is only consulted when that RPC query can't produce an answer
    /// (timeout, dead session) or reports something that isn't a
    /// diffable real file (an unnamed/scratch buffer). Without it, either
    /// of those cases would have nothing to fall back to at all.
    nvim_current_file: Option<std::path::PathBuf>,
    /// Whether the "comments require nvim mode" stderr note has already
    /// been printed this run -- [`Self::comment_node`] prints it at most
    /// once (repeating it on every `c` press outside nvim mode would just
    /// be noise) rather than tracking it any more elaborately.
    warned_comments_need_nvim: bool,
    /// Whether the "commenting requires the vdiff.nvim plugin" stderr hint
    /// has already been printed this run -- distinct from
    /// [`Self::warned_comments_need_nvim`] (that one covers "nvim mode isn't
    /// even on"; this one covers "nvim mode is on, but the plugin that
    /// actually handles `c` isn't installed") since a user hitting each one
    /// needs to fix a different thing. See [`Self::comment_node`].
    warned_missing_comment_plugin: bool,
    /// The whole loaded `<git_dir>/vdiff/review-state.json` (every branch,
    /// not just this one) -- kept around so [`Self::persist_review_state`]
    /// can replace just [`Self::review_branch`]'s entry via
    /// [`crate::review::review_state::ReviewStore::set_branch`] without
    /// clobbering any other branch's saved progress.
    review_store: crate::review::review_state::ReviewStore,
    /// The current branch name (or `"HEAD"` if detached -- see
    /// [`GitRepo::current_branch`]), keying [`Self::review_store`].
    review_branch: String,
    /// [`GraphViewCache`]'s expensive-to-recompute, `graph`/`show_tests`-
    /// derived data for [`graph_view::show`] -- built once in [`Self::new`]
    /// and rebuilt in [`Self::execute`]'s [`Cmd::Relayout`] arm, the only
    /// two moments either input can change. See its own doc for why this
    /// isn't just recomputed inline in `show` on every repaint.
    graph_view_cache: GraphViewCache,
}

/// Everything [`VdiffApp::new`] needs to set up (and later respawn) the
/// embedded-nvim spike, bundled to keep the constructor's arg count sane.
/// `pane` is `None` when nvim mode is off (`--no-nvim` was given) or no
/// `nvim` binary was found -- see `main.rs`'s startup decision -- in which
/// case the rest of the fields are unused.
pub struct NvimConfig {
    /// An already-spawned embedded session, or `None` to run in the
    /// built-in-viewer-only mode this struct otherwise doesn't touch.
    pub pane: Option<NvimPane>,
    /// `nvim --embed`'s working directory, remembered for
    /// [`VdiffApp::respawn_nvim`] (the initial spawn happens in `main.rs`,
    /// before a `VdiffApp` exists to remember it itself).
    pub cwd: std::path::PathBuf,
    /// Ex commands to run after every `nvim_ui_attach` -- the initial spawn
    /// and every respawn (see `--nvim-cmd` in `main.rs`'s CLI).
    pub init_cmds: Vec<String>,
    /// The egui context, kept around so [`VdiffApp::respawn_nvim`] can spawn
    /// a fresh [`NvimPane`] (which needs a `Context` to request repaints)
    /// outside of a paint callback.
    pub egui_ctx: Context,
}

/// Everything [`VdiffApp::new`] needs to persist review-completion state
/// (issue #4): the whole loaded store (every branch, so
/// [`VdiffApp::persist_review_state`] can save back just this run's branch
/// entry without touching any other) and which branch this run is on.
/// `App::reviewed` itself (already seeded/invalidated by the caller before
/// `App` was even built) is not part of this -- it lives on [`core::App`],
/// the one piece of review state `core` does own.
pub struct ReviewConfig {
    /// The store loaded at startup via
    /// [`crate::review::store::load_review_state`].
    pub store: ReviewStore,
    /// The current branch name (see [`GitRepo::current_branch`]).
    pub branch: String,
}

impl VdiffApp {
    /// Build a fresh GUI app wrapping an already-constructed [`App`] and its
    /// [`LayoutResult`]. `smoke` enables the self-closing startup self-test
    /// (see the module-level `--smoke` flag in `main.rs`). `diff_loader`
    /// backs [`Cmd::LoadDiff`]. `nvim` carries everything the embedded-nvim
    /// spike needs -- see [`NvimConfig`]'s doc.
    pub fn new(
        app: App,
        layout: LayoutResult,
        smoke: bool,
        diff_loader: DiffLoader,
        nvim: NvimConfig,
        review: ReviewConfig,
    ) -> Self {
        let graph_view_cache = GraphViewCache::rebuild(&app);
        Self {
            app,
            layout,
            transform: Transform::initial(),
            last_focus: None,
            initial_view_centered: false,
            pending_key: None,
            smoke,
            started_at: Instant::now(),
            diff_loader,
            nvim: nvim.pane,
            nvim_ctrl_w_pending: false,
            nvim_cwd: nvim.cwd,
            nvim_init_cmds: nvim.init_cmds,
            egui_ctx: nvim.egui_ctx,
            nvim_current_file: None,
            warned_comments_need_nvim: false,
            warned_missing_comment_plugin: false,
            review_store: review.store,
            review_branch: review.branch,
            graph_view_cache,
        }
    }

    /// Dispatch `msg` through the pure reducer and execute the resulting
    /// [`Cmd`]. Clones the whole [`App`] on every call so `update` can stay
    /// a pure `App -> (App, Cmd)` function with no `&mut` anywhere in
    /// `core` -- a deliberate trade-off (`App` is small and this runs at
    /// most a few times per user keypress, not per frame) rather than an
    /// oversight.
    fn dispatch(&mut self, msg: Msg) {
        let (app, cmd) = update(self.app.clone(), msg);
        self.app = app;
        self.execute(cmd);
    }

    /// Execute a [`Cmd`]: `LoadDiff` reads file content via `diff_loader`
    /// and reports the result back through the reducer as `DiffLoaded`/
    /// `LoadFailed`; `Relayout` rebuilds `self.layout` from
    /// [`App::visible_graph`] now that `self.app.layers` changed shape, and
    /// rebuilds [`Self::graph_view_cache`] alongside it (see
    /// [`GraphViewCache`]'s doc).
    fn execute(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::None => {}
            Cmd::LoadDiff(node) => match self.diff_loader.load(&self.app.graph, &node) {
                Ok(state) => self.dispatch(Msg::DiffLoaded(state)),
                Err(message) => self.dispatch(Msg::LoadFailed(message)),
            },
            Cmd::LoadFile(node) => self.load_file(node),
            Cmd::Relayout => {
                // `graph`/`show_tests` are exactly what changed to produce
                // this `Cmd::Relayout` (see `core::app::toggle_tests`), so
                // `GraphViewCache` needs rebuilding here too -- see its own
                // doc for why `graph_view::show` doesn't just recompute it
                // itself every frame. Rebuilt first so `layout_with_test_strips`
                // below reuses its `strips` instead of computing them twice.
                self.graph_view_cache = GraphViewCache::rebuild(&self.app);
                self.layout = layout_with_test_strips(
                    &self.app.visible_graph(),
                    &self.graph_view_cache.strips,
                );
            }
            Cmd::CommentNode(node) => self.comment_node(node),
            Cmd::PersistReviewState => self.persist_review_state(),
        }
    }

    /// Handle [`Cmd::PersistReviewState`]: recompute
    /// [`Self::review_branch`]'s entry from the current
    /// `self.app.graph`/`self.app.reviewed` (see
    /// [`crate::review::review_state::capture`]), fold it into
    /// [`Self::review_store`] (leaving every other branch's entry alone),
    /// and write the whole store back to
    /// `<git_dir>/vdiff/review-state.json`. A write failure is logged, not
    /// fatal -- losing this save just means the next toggle tries again;
    /// there's nothing else in `App` that depends on it having succeeded.
    fn persist_review_state(&mut self) {
        let captured = crate::review::review_state::capture(&self.app.graph, &self.app.reviewed);
        self.review_store.set_branch(&self.review_branch, captured);
        let git_dir = self.diff_loader.repo.git_dir();
        if let Err(err) = review_store::save_review_state(&git_dir, &self.review_store) {
            eprintln!(
                "warning: failed to save {}: {err}",
                review_store::review_state_path(&git_dir).display()
            );
        }
    }

    /// Handle [`Cmd::LoadFile`]: in nvim mode, respawn first if the session
    /// died since it was last used (see [`Self::respawn_nvim`]), then load
    /// the real [`FileViewState`] (the same load the built-in viewer uses
    /// -- its `changed_ranges`/`deleted` are exactly what
    /// [`NvimPane::open_file`] needs to mark the buffer, so there's no
    /// separate "nvim path" computation to keep in sync) and open the
    /// first file in the session, tracking it in
    /// [`Self::nvim_current_file`] for `:VdiffDiff`/`d`. Otherwise the
    /// built-in viewer's load, unchanged -- same [`Msg::FileLoaded`]/
    /// [`Msg::FileLoadFailed`] either way.
    fn load_file(&mut self, node: NodeId) {
        if self.nvim.is_some() {
            if self.nvim.as_ref().is_some_and(|nvim| !nvim.is_alive()) {
                self.respawn_nvim();
            }
            match self.diff_loader.load_file_view(&self.app.graph, &node) {
                Ok(state) => {
                    self.nvim_current_file = self
                        .app
                        .graph
                        .node(&node)
                        .and_then(|module| module.files.first())
                        .map(|file_ref| file_ref.path.clone());
                    if let (Some(nvim), Some(file)) = (&self.nvim, state.current_file()) {
                        nvim.open_file(file.path.clone(), Some(1), file.changed_ranges.clone());
                    }
                    self.dispatch(Msg::FileLoaded(state));
                }
                Err(message) => self.dispatch(Msg::FileLoadFailed(message)),
            }
            return;
        }
        match self.diff_loader.load_file_view(&self.app.graph, &node) {
            Ok(state) => self.dispatch(Msg::FileLoaded(state)),
            Err(message) => self.dispatch(Msg::FileLoadFailed(message)),
        }
    }

    /// Handle [`Cmd::CommentNode`]: in nvim mode, open the node's file (via
    /// [`Self::load_file`] -- same as `Enter`/[`Msg::OpenFile`], so the
    /// same marks/tracking apply) and delegate the actual comment-compose
    /// flow to `vdiff.nvim` via [`NvimPane::delegate_comment_node`] -- this
    /// app no longer owns comment capture at all (compose UI, writing
    /// `comments.json`, rendering comment extmarks all moved to that
    /// standalone plugin, which loads inside the embedded session
    /// automatically since it runs the user's own nvim config).
    ///
    /// Two distinct fallbacks, kept separate: outside nvim mode entirely
    /// there's no embedded session to delegate to at all, so this keeps the
    /// pre-existing "comments require nvim mode" note (unchanged, still
    /// gated by [`Self::warned_comments_need_nvim`]); inside nvim mode but
    /// with `vdiff.nvim` not installed (delegation returns `false` -- `require`
    /// failed, or the module has no `comment_range`), it prints the plugin
    /// hint instead (gated by [`Self::warned_missing_comment_plugin`],
    /// separately, since these are different diagnoses a user would fix
    /// differently).
    fn comment_node(&mut self, node: NodeId) {
        if self.nvim.is_none() {
            if !self.warned_comments_need_nvim {
                eprintln!("note: comments require nvim mode (see --no-nvim)");
                self.warned_comments_need_nvim = true;
            }
            return;
        }
        self.load_file(node.clone());
        let delegated = self
            .nvim
            .as_ref()
            .is_some_and(|nvim| nvim.delegate_comment_node(&node.to_string()));
        if !delegated && !self.warned_missing_comment_plugin {
            eprintln!(
                "vdiff: commenting requires the vdiff.nvim plugin (github.com/jowi-dev/vdiff.nvim)"
            );
            self.warned_missing_comment_plugin = true;
        }
    }

    /// Tear down the dead session (dropping it kills/reaps the child --
    /// see [`crate::nvim::session::NvimSession`]'s `Drop`) and spawn a
    /// fresh one at the same size, re-running [`Self::nvim_init_cmds`]. On
    /// spawn failure, logs a warning and leaves `self.nvim` as `None` --
    /// [`Self::load_file`]'s nvim branch is skipped from then on, so the
    /// rest of this run falls back to the built-in viewer.
    fn respawn_nvim(&mut self) {
        let (cols, rows) = self.nvim.as_ref().map_or((80, 24), NvimPane::size);
        self.nvim = None; // drop the old session (kills/reaps the child) before spawning the new one.
        match NvimPane::spawn(&self.nvim_cwd, cols, rows, self.egui_ctx.clone()) {
            Ok(pane) => {
                pane.register_vdiff_commands();
                for cmd in &self.nvim_init_cmds {
                    if let Err(message) = pane.run_init_command(cmd) {
                        eprintln!("warning: {message}");
                    }
                }
                self.nvim = Some(pane);
            }
            Err(err) => {
                eprintln!("warning: failed to respawn nvim: {err}");
            }
        }
    }

    /// Read this frame's key-press events, translate and route each through
    /// [`map_key`], updating the pending chord char and dispatching any
    /// resulting [`Msg`]. In nvim mode with keyboard focus on
    /// [`Pane::File`], delegates to [`Self::handle_nvim_keys`] instead --
    /// see that method's doc. In nvim mode with focus on [`Pane::Graph`],
    /// bare `d` (no chord in progress, no picker open) is intercepted
    /// before it ever reaches `map_key` -- see
    /// [`Self::should_open_nvim_diff`] -- and opens the focused node's
    /// file in the nvim pane plus its diffsplit in one step, rather than
    /// `map_key`'s normal `Msg::OpenDiff` (the full-screen built-in diff
    /// view, which stays reachable via `d` exactly as before whenever nvim
    /// mode is off).
    fn handle_keys(&mut self, ctx: &Context) {
        if self.nvim.is_some() && self.app.pane == Pane::File {
            self.handle_nvim_keys(ctx);
            return;
        }
        let presses = ctx.input(|i| extract_key_presses(&i.events));

        for (key, modifiers) in presses {
            let Some(input) = egui_key_to_input(key, modifiers) else {
                continue;
            };
            if self.should_open_nvim_diff(input) {
                self.open_nvim_diff_for_focus();
                continue;
            }
            let ctx = KeyContext {
                screen: self.app.screen,
                pane: self.app.pane,
                file_open: self.app.file_view.is_some(),
                picker_open: self.app.picker.is_some(),
                pending: self.pending_key,
            };
            let outcome = map_key(input, ctx);
            self.pending_key = None;
            match outcome {
                KeyOutcome::Msg(msg) => self.dispatch(msg),
                KeyOutcome::Pending(pending) => self.pending_key = Some(pending),
                KeyOutcome::None => {}
            }
        }
    }

    /// Whether `input` should be intercepted for the nvim-mode `d` ==
    /// "open in nvim + diffsplit" binding instead of `map_key`'s normal
    /// `Char('d')` -> `Msg::OpenDiff`: nvim mode active, on
    /// [`Screen::Graph`]/[`Pane::Graph`] (never [`Pane::File`] -- that's
    /// `handle_nvim_keys`'s territory and `d` there just forwards to nvim
    /// as `dd`/etc.), and no chord or picker mid-flight (so `gd`'s `d` and
    /// any other multi-key sequence ending in `d` are untouched). Pure
    /// given `self`'s relevant fields -- kept as a method rather than a
    /// free function since there's no value in threading five `&self`
    /// fields through a standalone signature for a one-call predicate.
    fn should_open_nvim_diff(&self, input: KeyInput) -> bool {
        input == KeyInput::Char('d')
            && self.nvim.is_some()
            && self.app.screen == Screen::Graph
            && self.app.pane == Pane::Graph
            && self.app.picker.is_none()
            && self.pending_key.is_none()
    }

    /// The nvim-mode `d` binding: open the focused node's file in the nvim
    /// pane exactly like `Enter` (via [`Msg::OpenFile`] -- same marks,
    /// same [`Self::nvim_current_file`] tracking), then immediately run
    /// the same diffsplit [`Self::trigger_vdiff_diff`] runs for
    /// `:VdiffDiff`, so the result is graph focus landing directly on the
    /// nvim pane already in diff mode against the merge-base version --
    /// one keystroke instead of open-then-`:VdiffDiff`.
    fn open_nvim_diff_for_focus(&mut self) {
        self.dispatch(Msg::OpenFile);
        self.trigger_vdiff_diff();
    }

    /// The fix for the `ZZ` lockup's user-facing half: if the nvim pane has
    /// died (see [`NvimPane::is_alive`]) while it still holds keyboard
    /// focus, dispatch [`Msg::PaneLeft`] *before* this frame's key handling
    /// runs -- so a dead session can never trap the user on [`Pane::File`]
    /// with no working keys to escape with (previously, `Ctrl-w h` was the
    /// only way out, and nothing told the user that). A no-op once focus is
    /// already back on the graph, or the pane never died.
    fn reclaim_focus_from_dead_nvim(&mut self) {
        let dead = self.nvim.as_ref().is_some_and(|nvim| !nvim.is_alive());
        if dead && self.app.pane == Pane::File {
            self.dispatch(Msg::PaneLeft);
        }
    }

    /// Drain any `:VdiffDiff` invocations that arrived since last frame
    /// (see [`NvimPane::take_diff_request`] -- fed by the embedded
    /// `:VdiffDiff` Ex command's `rpcnotify`) and run the diffsplit flow
    /// once for however many piled up.
    fn poll_vdiff_diff_requests(&mut self) {
        let requested = self.nvim.as_ref().is_some_and(NvimPane::take_diff_request);
        if requested {
            self.trigger_vdiff_diff();
        }
    }

    /// Drain any `vdiff_comment_saved` notifications that arrived since
    /// last frame (see [`NvimPane::take_comment_saved`] -- fed by
    /// `vdiff.nvim`'s compose UI `rpcnotify`-ing this host once a comment
    /// is written) and reload+remap the comment store (issue #14) once for
    /// however many piled up, so a comment captured mid-session shows up
    /// as a graph badge without a restart.
    fn poll_comment_saved(&mut self) {
        let saved = self.nvim.as_ref().is_some_and(NvimPane::take_comment_saved);
        if saved {
            self.reload_comments();
        }
    }

    /// Reload `<git_dir>/vdiff/comments.json` and remap it onto
    /// [`App::graph`], replacing [`App::comments`] wholesale -- the same
    /// load-then-map [`crate::review::comments::map_comments`] does at
    /// startup, just re-run on demand. A read/parse failure is logged and
    /// leaves the previous badges in place rather than clearing them: a
    /// transient error mid-session shouldn't make comments a reviewer
    /// already saw disappear from the graph.
    fn reload_comments(&mut self) {
        let git_dir = self.diff_loader.repo.git_dir();
        match review_store::load(&git_dir) {
            Ok(comments) => {
                self.app.comments = map_comments(&self.app.graph, &comments);
            }
            Err(err) => {
                eprintln!(
                    "warning: failed to reload {}: {err}",
                    review_store::comments_path(&git_dir).display()
                );
            }
        }
    }

    /// The diffsplit-against-merge-base flow itself, shared by
    /// `:VdiffDiff` (typed inside nvim) and the nvim-mode `d` binding
    /// (typed from the graph). Delegates to
    /// [`crate::nvim::vdiff_glue::trigger_diffsplit`] (via
    /// [`NvimPane::trigger_diffsplit`]) for the frontend-neutral resolution
    /// logic -- nvim's *actual* current buffer first, falling back to
    /// [`Self::nvim_current_file`] only when that query can't produce a
    /// usable answer (RPC timeout/dead session, or the current buffer is
    /// unnamed/one of this plugin's own `vdiff-base://` scratch buffers --
    /// see [`crate::nvim::vdiff_glue::resolve_diffed_path`]'s doc for the
    /// full list). `nvim_current_file` is only ever written when graph
    /// navigation opens a file, so trusting it unconditionally (instead of
    /// as a fallback) would risk diffing the file last opened that way
    /// rather than whatever the user `:e`'d or `Ctrl-w w`'d to inside nvim
    /// itself -- a plausible-looking wrong diff.
    ///
    /// The injected closure reads the resolved path's base content via
    /// [`DiffLoader`]'s repo/base_oid (already held for the built-in diff
    /// pane). This works for *any* file in the repo, not just ones backing
    /// a graph node -- reviewing a file reached by navigating inside nvim
    /// is still a valid diff request. A missing base blob (an added file)
    /// reads back `None`/empty content -- diffing against an empty buffer
    /// is the correct, unsurprising result, not an error. Prints a stderr
    /// warning if neither the query nor the fallback produced a path
    /// (shouldn't happen in practice, but the notification path can't
    /// assume the glue's state didn't move on by the time it's processed).
    fn trigger_vdiff_diff(&mut self) {
        let Some(nvim) = &self.nvim else { return };
        let fallback = self.nvim_current_file.clone();
        let diff_loader = &self.diff_loader;
        let sent = nvim.trigger_diffsplit(&self.nvim_cwd, fallback, |path| {
            diff_loader
                .repo
                .base_blob(&diff_loader.base_oid, path)
                .unwrap_or(None)
        });
        if !sent {
            eprintln!("warning: :VdiffDiff requested with no file open in the nvim pane");
        }
    }

    /// The nvim-mode input path: hands this frame's raw egui events to the
    /// pure [`nvim_pane::process_nvim_events`] (see that function's doc for
    /// why the `Ctrl-w` chord specifically needs to be handled there,
    /// against real event sequences, rather than per-event here), stores
    /// the chord-armed state it returns for next frame, then executes each
    /// resulting [`NvimAction`] -- forwarding input as-is, or resolving a
    /// boundary check via [`NvimPane::at_boundary`] (impure -- an RPC round
    /// trip -- which is why it isn't done inside the pure function itself).
    /// `map_key`/the reducer are bypassed entirely for everything else --
    /// nvim owns its own modal keymap, and `core::App`'s `File*` messages
    /// (scroll, half-page, change/file jump) have nothing to act on here
    /// since there's no [`FileViewState`] content backing this pane.
    fn handle_nvim_keys(&mut self, ctx: &Context) {
        let events = ctx.input(|i| i.events.clone());
        let (actions, pending) = nvim_pane::process_nvim_events(&events, self.nvim_ctrl_w_pending);
        self.nvim_ctrl_w_pending = pending;
        for action in actions {
            self.execute_nvim_action(action);
        }
    }

    /// Execute one [`NvimAction`] from [`Self::handle_nvim_keys`]. A no-op
    /// if the nvim pane isn't actually live (shouldn't happen --
    /// `handle_nvim_keys` is only called while `self.nvim.is_some()` -- but
    /// defended rather than assumed).
    fn execute_nvim_action(&mut self, action: NvimAction) {
        let Some(nvim) = &self.nvim else { return };
        match action {
            NvimAction::Input(text) => nvim.send(NvimCmd::Input(text)),
            NvimAction::CtrlWBoundary {
                dir,
                hop_left,
                forward_seq,
            } => {
                if nvim.at_boundary(dir) {
                    if hop_left {
                        self.dispatch(Msg::PaneLeft);
                    }
                    // At the right boundary already: nothing further right
                    // to hop to (there's no pane past the nvim pane).
                } else {
                    nvim.send(NvimCmd::Input(forward_seq.to_string()));
                }
            }
        }
    }

    /// `+`/`=` zoom in, `-` zooms out, both anchored on the focused node's
    /// rect center. This is view-only: it only ever changes
    /// [`Self::transform`], never [`core::App`] state, so -- unlike
    /// [`Self::handle_keys`] -- it bypasses [`map_key`]/the reducer
    /// entirely instead of adding these as [`crate::keymap::KeyInput`]
    /// variants. Only meaningful on [`Screen::Graph`], where a `Transform`
    /// and focused node's layout rect exist.
    fn handle_zoom_keys(&mut self, ctx: &Context) {
        if self.app.screen != Screen::Graph {
            return;
        }
        let presses: Vec<Key> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key: key @ (Key::Plus | Key::Equals | Key::Minus),
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
                .collect()
        });
        if presses.is_empty() {
            return;
        }
        let Some(focus_rect) = self.layout.rects.get(&self.app.focus) else {
            return;
        };
        let anchor = self.transform.to_screen_pos(focus_rect.center());
        for key in presses {
            let factor = if key == Key::Minus {
                1.0 / ZOOM_KEY_FACTOR
            } else {
                ZOOM_KEY_FACTOR
            };
            self.transform.zoom(factor, anchor);
        }
    }

    /// Floating j/k/Enter/Esc picker for [`crate::core::app::Msg::FollowDeps`]/
    /// [`crate::core::app::Msg::FollowDependents`], listing candidate
    /// display names with the selected one highlighted. Fixed/centered, not
    /// user-movable -- matches the project's floating-overlay convention.
    fn show_picker(&self, ctx: &Context) {
        let Some(picker) = &self.app.picker else {
            return;
        };
        egui::Window::new("Jump to")
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .resizable(false)
            .collapsible(false)
            .title_bar(true)
            .show(ctx, |ui| {
                for (i, candidate) in picker.candidates.iter().enumerate() {
                    let name = self
                        .app
                        .graph
                        .node(candidate)
                        .map(|n| n.display_name.as_str())
                        .unwrap_or("?");
                    let _ = ui.selectable_label(i == picker.selected, name);
                }
            });
    }

    /// [`Screen::Diff`]: the loaded diff pane via [`diff_view::show`], or a
    /// loading message while [`Cmd::LoadDiff`] is still in flight.
    fn show_diff(&self, ui: &mut egui::Ui) {
        match &self.app.diff {
            Some(diff) => diff_view::show(ui, diff),
            None => {
                ui.heading("Loading diff...");
            }
        }
    }

    /// [`Screen::Graph`]: the graph always fills the whole viewport now --
    /// no side panel, ever (see [`overlay`]'s module doc for the
    /// rationale behind replacing the earlier 50%-width side panel with
    /// this). [`Pane::File`] with a file open instead paints
    /// [`overlay::show`] fullscreen on top of the already-painted graph --
    /// an opaque header strip, then either the nvim grid or the built-in
    /// [`crate::ui::file_view::show`] beneath it, its own background
    /// painted translucent rather than sitting under a separate scrim (see
    /// [`overlay`]'s doc again). Records the row count [`overlay::show`]
    /// reports back (only
    /// meaningful in built-in mode -- nvim's `Ctrl-d`/`Ctrl-u` are
    /// forwarded raw, see [`Self::handle_nvim_keys`]) as
    /// [`App::viewport_rows`], read one frame later by
    /// [`Msg::FileHalfPage`] -- see that field's doc for why this can't be
    /// computed any earlier.
    fn show_graph_screen(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            if !self.initial_view_centered {
                if let Some(graph_width) = graph_view::graph_width(&self.layout) {
                    self.transform.offset.x = graph_view::initial_x_offset(
                        ui.max_rect().width(),
                        graph_width,
                        self.transform.scale,
                        crate::ui::theme::GRAPH_TOP_PADDING,
                    );
                }
                self.initial_view_centered = true;
            }
            graph_view::show(
                ui,
                &self.app,
                &self.layout,
                &mut self.transform,
                &mut self.last_focus,
                &self.graph_view_cache,
            );
            if self.app.pane == Pane::File {
                if let Some(file_view) = self.app.file_view.as_ref() {
                    let viewport_rows = overlay::show(ui, &self.app, file_view, self.nvim.as_mut());
                    if let Some(rows) = viewport_rows {
                        self.app.viewport_rows = rows;
                    }
                }
            }
        });
        self.show_picker(&ctx);
    }
}

impl eframe::App for VdiffApp {
    /// Non-painting logic, called once before [`Self::ui`] each frame: the
    /// `--smoke` self-close timer and key-event handling.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if self.smoke {
            if self.started_at.elapsed() > SMOKE_DURATION {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        self.reclaim_focus_from_dead_nvim();
        self.poll_vdiff_diff_requests();
        self.poll_comment_saved();
        self.handle_keys(ctx);
        self.handle_zoom_keys(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.app.screen {
            Screen::Graph => self.show_graph_screen(ui),
            Screen::Diff => {
                egui::CentralPanel::default().show(ui, |ui| {
                    self.show_diff(ui);
                });
            }
        }
    }
}

/// Filter this frame's raw egui events down to actual key *presses*
/// (`pressed: true`, `repeat: false`), discarding releases and everything
/// that isn't [`Event::Key`] (in particular `Event::Text`, which
/// [`egui_key_to_input`]/`map_key`'s discrete-key chords never consult).
/// This is what makes the built-in-viewer/graph-side `Ctrl-w h`/`Ctrl-w l`
/// chord immune to the bug [`crate::ui::nvim_pane::process_nvim_events`]'s
/// doc describes for the nvim-mode chord: releases (including `Ctrl-w`'s
/// own) never even reach [`map_key`]'s pending-chord check here, so they
/// can't disarm it, and there's no `Text` forwarding path to leak through
/// in the first place.
fn extract_key_presses(events: &[egui::Event]) -> Vec<(Key, Modifiers)> {
    events
        .iter()
        .filter_map(|event| match event {
            egui::Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } => Some((*key, *modifiers)),
            _ => None,
        })
        .collect()
}

/// Translate an egui key press (with its modifiers) to vdiff's
/// toolkit-independent [`KeyInput`]. Pure and unit-tested: with Ctrl held,
/// only `w`/`d`/`u` map to anything ([`KeyInput::Ctrl`]); otherwise the
/// keys [`crate::keymap::map_key`] cares about (h/j/k/l/g/G/d/r/t/s/c/f/[/],
/// Enter, Esc) map to anything, arrows map to [`KeyInput::Arrow`]
/// unconditionally (checked before the Ctrl branch, so `Ctrl-w` followed by
/// an arrow -- held or released -- both complete the `Ctrl-w` chord the
/// same way `Ctrl-w h`/`Ctrl-w l` do; see [`crate::keymap::resolve_pending`]),
/// everything else is `None`. `Key::G` maps to `Char('G')` when Shift is
/// held (uppercase, distinct from the `gg`/`gd`/`gr` prefix `Char('g')`)
/// and `Char('g')` otherwise.
pub fn egui_key_to_input(key: Key, modifiers: Modifiers) -> Option<KeyInput> {
    if let Some(dir) = arrow_direction(key) {
        return Some(KeyInput::Arrow(dir));
    }
    if modifiers.ctrl {
        return match key {
            Key::W => Some(KeyInput::Ctrl('w')),
            Key::D => Some(KeyInput::Ctrl('d')),
            Key::U => Some(KeyInput::Ctrl('u')),
            _ => None,
        };
    }
    match key {
        Key::H => Some(KeyInput::Char('h')),
        Key::J => Some(KeyInput::Char('j')),
        Key::K => Some(KeyInput::Char('k')),
        Key::L => Some(KeyInput::Char('l')),
        Key::G => Some(KeyInput::Char(if modifiers.shift { 'G' } else { 'g' })),
        Key::D => Some(KeyInput::Char('d')),
        Key::R => Some(KeyInput::Char('r')),
        Key::T => Some(KeyInput::Char('t')),
        Key::S => Some(KeyInput::Char('s')),
        Key::C => Some(KeyInput::Char('c')),
        Key::V => Some(KeyInput::Char('v')),
        Key::F => Some(KeyInput::Char('f')),
        Key::OpenBracket => Some(KeyInput::Char('[')),
        Key::CloseBracket => Some(KeyInput::Char(']')),
        Key::Enter => Some(KeyInput::Enter),
        Key::Escape => Some(KeyInput::Esc),
        _ => None,
    }
}

/// `Key::Arrow*` to [`Direction`], or `None` for any other key.
fn arrow_direction(key: Key) -> Option<Direction> {
    match key {
        Key::ArrowLeft => Some(Direction::Left),
        Key::ArrowRight => Some(Direction::Right),
        Key::ArrowUp => Some(Direction::Up),
        Key::ArrowDown => Some(Direction::Down),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::repo::FakeRepo;

    #[test]
    fn translates_mapped_keys_with_no_modifiers() {
        let cases = [
            (Key::H, KeyInput::Char('h')),
            (Key::J, KeyInput::Char('j')),
            (Key::K, KeyInput::Char('k')),
            (Key::L, KeyInput::Char('l')),
            (Key::G, KeyInput::Char('g')),
            (Key::D, KeyInput::Char('d')),
            (Key::R, KeyInput::Char('r')),
            (Key::T, KeyInput::Char('t')),
            (Key::S, KeyInput::Char('s')),
            (Key::C, KeyInput::Char('c')),
            (Key::V, KeyInput::Char('v')),
            (Key::F, KeyInput::Char('f')),
            (Key::OpenBracket, KeyInput::Char('[')),
            (Key::CloseBracket, KeyInput::Char(']')),
            (Key::Enter, KeyInput::Enter),
            (Key::Escape, KeyInput::Esc),
        ];
        for (key, expected) in cases {
            assert_eq!(
                egui_key_to_input(key, Modifiers::NONE),
                Some(expected),
                "key={key:?}"
            );
        }
    }

    #[test]
    fn shift_g_translates_to_uppercase() {
        assert_eq!(
            egui_key_to_input(Key::G, Modifiers::SHIFT),
            Some(KeyInput::Char('G'))
        );
    }

    #[test]
    fn ctrl_modifier_maps_w_d_u_and_nothing_else() {
        assert_eq!(
            egui_key_to_input(Key::W, Modifiers::CTRL),
            Some(KeyInput::Ctrl('w'))
        );
        assert_eq!(
            egui_key_to_input(Key::D, Modifiers::CTRL),
            Some(KeyInput::Ctrl('d'))
        );
        assert_eq!(
            egui_key_to_input(Key::U, Modifiers::CTRL),
            Some(KeyInput::Ctrl('u'))
        );
        assert_eq!(egui_key_to_input(Key::H, Modifiers::CTRL), None);
    }

    #[test]
    fn unmapped_keys_translate_to_none() {
        for key in [Key::A, Key::Z, Key::Num1, Key::Space, Key::Tab] {
            assert_eq!(egui_key_to_input(key, Modifiers::NONE), None, "key={key:?}");
        }
    }

    #[test]
    fn arrow_keys_translate_to_arrow_input_with_or_without_ctrl() {
        let cases = [
            (Key::ArrowLeft, Direction::Left),
            (Key::ArrowRight, Direction::Right),
            (Key::ArrowUp, Direction::Up),
            (Key::ArrowDown, Direction::Down),
        ];
        for (key, dir) in cases {
            assert_eq!(
                egui_key_to_input(key, Modifiers::NONE),
                Some(KeyInput::Arrow(dir)),
                "key={key:?} (no modifiers)"
            );
            assert_eq!(
                egui_key_to_input(key, Modifiers::CTRL),
                Some(KeyInput::Arrow(dir)),
                "key={key:?} (ctrl held)"
            );
        }
    }

    #[test]
    fn extract_key_presses_drops_releases_and_text_from_a_real_ctrl_w_l_sequence() {
        // The exact event sequence a physical Ctrl-w, release, l, release
        // produces (including l's paired Text event) -- the graph-side
        // chord must see only the two presses, in order, with everything
        // else (both releases, and l's Text) filtered out.
        let events = vec![
            egui::Event::Key {
                key: Key::W,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
            egui::Event::Key {
                key: Key::W,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
            egui::Event::Key {
                key: Key::L,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            egui::Event::Text("l".to_string()),
            egui::Event::Key {
                key: Key::L,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ];

        assert_eq!(
            extract_key_presses(&events),
            vec![(Key::W, Modifiers::CTRL), (Key::L, Modifiers::NONE)]
        );
    }

    #[test]
    fn ctrl_w_then_l_completes_the_graph_side_chord_across_a_real_event_sequence() {
        // End-to-end through the same pipeline `handle_keys` drives:
        // extract_key_presses -> egui_key_to_input -> map_key. A file pane
        // must be open for `Ctrl-w l` to resolve to PaneRight (see
        // `KeyContext::file_open`).
        let events = vec![
            egui::Event::Key {
                key: Key::W,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
            egui::Event::Key {
                key: Key::W,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Modifiers::CTRL,
            },
            egui::Event::Key {
                key: Key::L,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
            egui::Event::Text("l".to_string()),
        ];

        let mut ctx = KeyContext {
            screen: Screen::Graph,
            pane: Pane::Graph,
            file_open: true,
            picker_open: false,
            pending: None,
        };
        let mut outcomes = Vec::new();
        for (key, modifiers) in extract_key_presses(&events) {
            let Some(input) = egui_key_to_input(key, modifiers) else {
                continue;
            };
            let outcome = map_key(input, ctx);
            ctx.pending = None;
            if let KeyOutcome::Pending(pending) = outcome {
                ctx.pending = Some(pending);
            }
            outcomes.push(outcome);
        }

        assert_eq!(
            outcomes,
            vec![
                KeyOutcome::Pending(Pending::CtrlW),
                KeyOutcome::Msg(Msg::PaneRight),
            ]
        );
    }

    /// One node backed by an added file (`new.rs`, no base content) and a
    /// modified file (`changed.rs`, differs on both sides) -- exercises
    /// `DiffLoader::load`'s added/modified branches against `FakeRepo`
    /// without needing a real git checkout.
    fn graph_and_repo() -> (ProjectGraph, FakeRepo) {
        use crate::graph::model::{FileRef, GitStatus, ModuleNode};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let node_id = NodeId::from("rust:demo");
        let node = ModuleNode {
            id: node_id.clone(),
            display_name: "demo".to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Modified,
            files: vec![
                FileRef {
                    path: PathBuf::from("new.rs"),
                    base_blob: None,
                    head_blob: Some("h1".to_string()),
                },
                FileRef {
                    path: PathBuf::from("changed.rs"),
                    base_blob: Some("b2".to_string()),
                    head_blob: Some("h2".to_string()),
                },
            ],
        };
        let mut nodes = HashMap::new();
        nodes.insert(node_id.clone(), node);
        let graph = ProjectGraph {
            nodes,
            roots: vec![node_id],
            edges: vec![],
        };

        let mut base_files = HashMap::new();
        base_files.insert(PathBuf::from("changed.rs"), "before\n".to_string());
        let mut head_files = HashMap::new();
        head_files.insert(PathBuf::from("new.rs"), "brand new\n".to_string());
        head_files.insert(PathBuf::from("changed.rs"), "after\n".to_string());

        let repo = FakeRepo {
            default_base_oid: "base-oid".to_string(),
            deltas: vec![],
            base_files,
            head_files,
            tracked_files: vec![],
            ..Default::default()
        };
        (graph, repo)
    }

    #[test]
    fn diff_loader_loads_every_file_with_correct_sides() {
        let (graph, repo) = graph_and_repo();
        let loader = DiffLoader {
            repo: Box::new(repo) as Box<dyn GitRepo>,
            base_oid: "base-oid".to_string(),
        };

        let state = loader.load(&graph, &NodeId::from("rust:demo")).unwrap();
        assert_eq!(state.node, NodeId::from("rust:demo"));
        assert_eq!(state.files.len(), 2);

        let new_file = state
            .files
            .iter()
            .find(|f| f.path.to_str() == Some("new.rs"))
            .unwrap();
        assert!(
            new_file.diff.base_lines.is_empty(),
            "added file has empty base"
        );
        assert_eq!(new_file.diff.head_lines, vec!["brand new"]);

        let changed_file = state
            .files
            .iter()
            .find(|f| f.path.to_str() == Some("changed.rs"))
            .unwrap();
        assert_eq!(changed_file.diff.base_lines, vec!["before"]);
        assert_eq!(changed_file.diff.head_lines, vec!["after"]);
        assert_eq!(changed_file.diff.hunks.len(), 1);
    }

    #[test]
    fn diff_loader_errors_on_unknown_node() {
        let (graph, repo) = graph_and_repo();
        let loader = DiffLoader {
            repo: Box::new(repo) as Box<dyn GitRepo>,
            base_oid: "base-oid".to_string(),
        };
        assert!(loader.load(&graph, &NodeId::from("nope")).is_err());
    }

    #[test]
    fn file_loader_loads_head_content_and_changed_ranges() {
        let (graph, repo) = graph_and_repo();
        let loader = DiffLoader {
            repo: Box::new(repo) as Box<dyn GitRepo>,
            base_oid: "base-oid".to_string(),
        };

        let state = loader
            .load_file_view(&graph, &NodeId::from("rust:demo"))
            .unwrap();
        assert_eq!(state.node, NodeId::from("rust:demo"));
        assert_eq!(state.files.len(), 2);

        let new_file = state
            .files
            .iter()
            .find(|f| f.path.to_str() == Some("new.rs"))
            .unwrap();
        assert_eq!(new_file.lines, vec!["brand new"]);
        assert!(!new_file.deleted);
        assert_eq!(
            new_file.changed_ranges,
            vec![(0, 0)],
            "whole added file is one changed range"
        );

        let changed_file = state
            .files
            .iter()
            .find(|f| f.path.to_str() == Some("changed.rs"))
            .unwrap();
        assert_eq!(changed_file.lines, vec!["after"]);
        assert!(!changed_file.deleted);
        assert_eq!(changed_file.changed_ranges, vec![(0, 0)]);
    }

    #[test]
    fn file_loader_errors_on_unknown_node() {
        let (graph, repo) = graph_and_repo();
        let loader = DiffLoader {
            repo: Box::new(repo) as Box<dyn GitRepo>,
            base_oid: "base-oid".to_string(),
        };
        assert!(loader
            .load_file_view(&graph, &NodeId::from("nope"))
            .is_err());
    }

    /// One node backed by a deleted file (`gone.rs`, base content only, no
    /// head blob) -- exercises `DiffLoader::load_file_view_entry`'s
    /// deleted-file fallback.
    fn graph_and_repo_with_deleted_file() -> (ProjectGraph, FakeRepo) {
        use crate::graph::model::{FileRef, GitStatus, ModuleNode};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let node_id = NodeId::from("rust:gone");
        let node = ModuleNode {
            id: node_id.clone(),
            display_name: "gone".to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Deleted,
            files: vec![FileRef {
                path: PathBuf::from("gone.rs"),
                base_blob: Some("b".to_string()),
                head_blob: None,
            }],
        };
        let mut nodes = HashMap::new();
        nodes.insert(node_id.clone(), node);
        let graph = ProjectGraph {
            nodes,
            roots: vec![node_id],
            edges: vec![],
        };

        let mut base_files = HashMap::new();
        base_files.insert(PathBuf::from("gone.rs"), "old content\n".to_string());

        let repo = FakeRepo {
            default_base_oid: "base-oid".to_string(),
            deltas: vec![],
            base_files,
            head_files: HashMap::new(),
            tracked_files: vec![],
            ..Default::default()
        };
        (graph, repo)
    }

    #[test]
    fn file_loader_deleted_file_falls_back_to_base_content_with_no_changed_ranges() {
        let (graph, repo) = graph_and_repo_with_deleted_file();
        let loader = DiffLoader {
            repo: Box::new(repo) as Box<dyn GitRepo>,
            base_oid: "base-oid".to_string(),
        };

        let state = loader
            .load_file_view(&graph, &NodeId::from("rust:gone"))
            .unwrap();
        let file = &state.files[0];
        assert!(file.deleted);
        assert_eq!(file.lines, vec!["old content"]);
        assert!(file.changed_ranges.is_empty(), "no head content to mark up");
    }
}
