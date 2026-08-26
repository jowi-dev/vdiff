//! Pure Elm-style state and reducer for vdiff's application core.
//!
//! [`App`] holds all state, [`Msg`] is every event [`update`] can react to,
//! and `update` is the single place state transitions happen. No I/O occurs
//! here: [`Cmd`] names the I/O the caller (the eframe glue, in a later
//! chunk) should perform next; `DiffLoaded`/`LoadFailed` feed its result back
//! in without `update` ever needing to touch git/egui itself.

use std::collections::{HashMap, HashSet};

pub use crate::core::diff_state::DiffPaneState;
use crate::core::file_view::FileViewState;
use crate::core::focus::{dep_targets, dependent_sources, move_focus, Direction};
use crate::core::rail_view::{self, RailDirection};
use crate::core::review;
use crate::graph::layout::{layout, rows_with_x_centers};
use crate::graph::model::{NodeId, ProjectGraph};
use crate::graph::test_modules::{
    group_matched_test_modules, hide_test_modules, matched_test_module,
};
use crate::review::comments::Comment;
use crate::review::findings::Finding;

/// Which screen is currently shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The graph screen: the node graph fills the whole viewport; once a
    /// file is opened, the file viewer takes over as a fullscreen overlay
    /// on top of it (see [`Pane`], and the rendering glue in
    /// `crate::ui::eframe_app`/`crate::ui::overlay` -- core has no notion
    /// of panes/panels/overlays, only which of the two logically has
    /// keyboard focus).
    Graph,
    /// The full-screen diff pane for the node focused when it was opened.
    Diff,
}

/// Which of [`Screen::Graph`]'s two modes has keyboard focus. Meaningful
/// only once [`App::file_view`] is `Some`; while it's `None` the graph has
/// the whole window and `pane` stays [`Pane::Graph`]. Purely a focus/state
/// concept in core -- how each renders (fullscreen graph vs. a fullscreen
/// editor overlay on top of it) is entirely the rendering glue's concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The node graph.
    Graph,
    /// The file viewer.
    File,
}

/// State for the floating j/k/Enter/Esc picker [`Msg::FollowDeps`]/
/// [`Msg::FollowDependents`] open when a node has more than one edge in the
/// requested direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgePicker {
    /// The candidate node ids, name-sorted (see
    /// [`crate::core::focus::dep_targets`]/[`dependent_sources`]).
    pub candidates: Vec<NodeId>,
    /// Index into `candidates` of the currently highlighted option.
    pub selected: usize,
}

/// All state needed to drive vdiff's core: the graph itself, what's
/// focused, which screen is shown, and any open overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// The project graph being browsed.
    pub graph: ProjectGraph,
    /// The layered-dependency-layout structure ([`crate::graph::layers::assign_layers`]'s
    /// output) `focus` navigation walks. Computed once by the caller when
    /// constructing `App` -- core stays geometry-free, but layering is pure
    /// graph logic, so it's fine for the caller to compute it up front and
    /// hand it in rather than `core` recomputing it from `graph` on every
    /// navigation step.
    pub layers: Vec<Vec<NodeId>>,
    /// [`crate::graph::layout::rows_with_x_centers`]'s output for the
    /// current `visible_graph()`'s layout: one entry per *visual* row (a
    /// wrapped layer contributes multiple rows here, unlike `layers`),
    /// each node paired with its rect's x-center. Computed once by the
    /// caller alongside `layers` (see `layers`'s own doc) and consulted by
    /// [`Msg::FocusMove`]'s `j`/`k` (via
    /// [`crate::core::focus::move_focus`]) so a wrapped sub-row navigates
    /// correctly instead of jumping a whole layer.
    pub rows: Vec<Vec<(NodeId, f32)>>,
    /// The currently focused node.
    pub focus: NodeId,
    /// The screen currently shown.
    pub screen: Screen,
    /// The diff pane's loaded state, `None` while [`Cmd::LoadDiff`] is still
    /// in flight (or the diff screen isn't open).
    pub diff: Option<DiffPaneState>,
    /// The edge-following picker overlay, `None` when closed.
    pub picker: Option<EdgePicker>,
    /// Whether Elixir/Rust test modules (see
    /// [`crate::graph::test_modules::is_test_module`]) are shown. Defaults
    /// to `false` -- half the nodes in a typical change set are test
    /// modules, and hiding them by default is what makes the layered graph
    /// read as a call-stack story rather than a wall of noise. Toggled by
    /// [`Msg::ToggleTests`] (`t` on [`Screen::Graph`]).
    pub show_tests: bool,
    /// The file viewer pane's loaded state, `None` while it's closed (or
    /// [`Cmd::LoadFile`] is still in flight for the very first open --
    /// [`Msg::OpenFile`] flips `pane` to [`Pane::File`] optimistically
    /// before the load completes, since the load itself is a local file
    /// read that finishes synchronously within the same dispatch).
    pub file_view: Option<FileViewState>,
    /// Which panel has keyboard focus on [`Screen::Graph`]. See [`Pane`].
    pub pane: Pane,
    /// The file pane's visible row count, fed in by the eframe glue each
    /// frame from the actual rendered height (a plain UI-measured input,
    /// not something `update` derives) -- [`Msg::FileHalfPage`] halves it
    /// for `Ctrl-d`/`Ctrl-u` scrolling. Defaults to 1 so half-page math
    /// never divides by (or scrolls by) zero before the first frame has
    /// measured anything.
    pub viewport_rows: usize,
    /// Ids marked reviewed via [`Msg::ToggleReviewed`] (`v` on a focused
    /// changed node in [`Pane::Graph`]). Seeded at startup by the eframe
    /// glue from `<git_dir>/vdiff/review-state.json`, already filtered
    /// through [`crate::core::review::invalidate`] against the current
    /// graph -- so by the time `App` exists, every id in here is known to
    /// still match the fingerprint it was marked reviewed under. `core`
    /// itself never reads the on-disk shape; it only ever grows/shrinks
    /// this set via [`review::toggle_reviewed`] and reports it back out
    /// through [`App::review_progress`].
    pub reviewed: HashSet<NodeId>,
    /// AI review findings (see [`crate::review::findings`]), already mapped
    /// onto node ids by [`crate::review::findings::map_findings`] once at
    /// startup from `--findings <path>` -- empty when the flag wasn't
    /// given. `core` never re-derives this from a node id/path itself;
    /// it's pure lookup data the rendering glue (graph badges, the focus
    /// overlay, the file pane) reads via [`App::findings_for`].
    pub findings: HashMap<NodeId, Vec<Finding>>,
    /// Review comments (see [`crate::review::comments`]), already mapped
    /// onto node ids by [`crate::review::comments::map_comments`] once at
    /// startup from `<git_dir>/vdiff/comments.json` -- empty when the store
    /// doesn't exist or has nothing in it. `core` never re-derives this
    /// from a node id/path itself; it's pure lookup data the rendering
    /// glue reads to paint the graph's comment badge (issue #14). Replaced
    /// wholesale, not incrementally patched, whenever the eframe glue
    /// reloads the store (e.g. after a `vdiff_comment_saved` notification
    /// from the embedded nvim session).
    pub comments: HashMap<NodeId, Vec<Comment>>,
    /// The `--tui` rail view's fold-by-namespace state (issue #16 phase 2):
    /// the set of namespace node ids currently collapsed to a single row.
    /// Empty by default -- the rail view opens fully expanded, matching its
    /// "big picture first" design (see `crate::core::rail_view`'s module
    /// doc for why this lives on `App` rather than as TUI-local state, the
    /// way [`crate::tui::TuiState::notice`] does). Always empty on the GUI
    /// path, which never collapses anything -- every
    /// [`crate::core::rail_view`] function is the identity transform when
    /// this is empty, so its presence has no GUI-visible effect.
    pub fold_collapsed: HashSet<NodeId>,
}

impl App {
    /// Whether `id` is a drawn (real, navigable) node -- present in
    /// `self.layers` -- as opposed to a synthetic namespace node or an
    /// unknown id. [`Msg::FocusSet`] rejects any target that isn't *this or*
    /// currently collapsed (see that variant's own doc for why a collapsed
    /// namespace id also has to pass).
    fn is_drawn(&self, id: &NodeId) -> bool {
        self.layers.iter().any(|layer| layer.contains(id))
    }

    /// `id`'s attached findings, or an empty slice if it has none -- the
    /// one lookup every findings-rendering call site (badge, focus
    /// overlay, file pane) should go through rather than matching on
    /// `self.findings.get(id)` directly.
    pub fn findings_for(&self, id: &NodeId) -> &[Finding] {
        self.findings.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The graph actually drawn: with every test module pruned out (see
    /// [`hide_test_modules`]) if `!self.show_tests`, or -- once
    /// [`Self::show_tests`] is on -- with only *matched* test modules
    /// pruned (see [`group_matched_test_modules`]), since those render as
    /// an attached strip on their tested node's box rather than a second
    /// standalone node (see [`crate::graph::test_modules::test_strips`],
    /// consulted by the caller alongside this to build that combined box).
    /// An unmatched test module is never pruned -- it stays a standalone
    /// node either way. `self.graph` itself never changes -- it's always
    /// the full, focus-filtered graph -- so this is what [`Msg::ToggleTests`]
    /// recomputes `layers`/`rows` from, and what the caller should re-run
    /// [`crate::graph::layout::layout_with_test_strips`] over after a
    /// [`Cmd::Relayout`].
    pub fn visible_graph(&self) -> ProjectGraph {
        if self.show_tests {
            group_matched_test_modules(&self.graph)
        } else {
            hide_test_modules(&self.graph).0
        }
    }

    /// "N/M changed modules reviewed" over the nodes currently drawn (see
    /// [`Self::layers`], which already reflects `show_tests`) -- delegates
    /// straight to [`review::review_progress`], the pure counting logic.
    pub fn review_progress(&self) -> (usize, usize) {
        review::review_progress(&self.graph, self.layers.iter().flatten(), &self.reviewed)
    }
}

/// Startup's `show_tests` seed: `false` (the normal default, matching
/// [`App::show_tests`]'s own default) unless the change set is *only*
/// test modules, in which case [`hide_test_modules`] would prune the
/// entire graph and vdiff would open on a blank canvas with a sentinel
/// focus (see `main::run_gui`, the only caller). Defined here rather than
/// inline at the call site so it's unit-testable as a pure decision over a
/// graph with no `App` (and no layout) yet in existence.
pub fn initial_show_tests(graph: &ProjectGraph) -> bool {
    !graph.nodes.is_empty() && hide_test_modules(graph).0.nodes.is_empty()
}

/// Every event [`update`] can react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// h/j/k/l layer navigation step (see [`crate::core::focus`]). Only
    /// acted on on [`Screen::Graph`] with no picker open.
    FocusMove(Direction),
    /// Jump focus directly to a node. Only acted on on [`Screen::Graph`]
    /// with no picker open, and only if the target is a drawn node (see
    /// [`App::is_drawn`]) *or* is currently a collapsed-namespace id (see
    /// [`App::fold_collapsed`]) -- the plane/canvas views' spatial `h`/`j`/
    /// `k`/`l` (`plane_key_msg`/`canvas_key_msg` in `crate::tui`) route
    /// through this variant, and their focus grids legitimately include
    /// collapsed-namespace rows as targets (a collapsed namespace is a
    /// real, visible row, not a dead id); rejecting those left plane/canvas
    /// hjkl silently and permanently unable to refocus a collapsed row once
    /// focus moved off it, with no way back except switching to the rail
    /// view. The rail view's own `Msg::RailFocusMove`/
    /// `Msg::CollapseFocusedNamespace` already set `App::focus` to a
    /// namespace id directly without going through this guard at all, and
    /// [`update`]'s central fold-aware remap keeps whichever id lands here
    /// consistent either way.
    FocusSet(NodeId),
    /// `gd`: follow the focused node's outgoing dependency edges.
    FollowDeps,
    /// `gr`: follow the focused node's incoming dependency edges.
    FollowDependents,
    /// Move the open picker's highlighted option by `delta`, clamped to the
    /// candidate list's bounds. A no-op if no picker is open.
    PickerMove(i32),
    /// Focus the picker's highlighted candidate and close it. A no-op if no
    /// picker is open.
    PickerSelect,
    /// Close the picker without changing focus. A no-op if no picker is
    /// open.
    PickerCancel,
    /// Open the diff pane for the focused node. Only acted on on
    /// [`Screen::Graph`] with no picker open.
    OpenDiff,
    /// Return to the graph screen, discarding any loaded diff state. Focus
    /// is preserved.
    CloseDiff,
    /// [`Cmd::LoadDiff`] succeeded.
    DiffLoaded(DiffPaneState),
    /// Scroll the diff pane's current file by `delta` rows, clamped. Only
    /// acted on with a loaded diff pane open.
    DiffScroll(i32),
    /// `]c`: jump to the next hunk in the current file. Only acted on with
    /// a loaded diff pane open.
    DiffNextHunk,
    /// `[c`: jump to the previous hunk in the current file. Only acted on
    /// with a loaded diff pane open.
    DiffPrevHunk,
    /// `s`: toggle side-by-side/unified rendering. Only acted on with a
    /// loaded diff pane open.
    DiffToggleMode,
    /// `]f`: switch to the next file backing the node, clamped. Only acted
    /// on with a loaded diff pane open.
    DiffNextFile,
    /// `[f`: switch to the previous file backing the node, clamped. Only
    /// acted on with a loaded diff pane open.
    DiffPrevFile,
    /// [`Cmd::LoadDiff`] failed; returns to the graph screen. The message is
    /// not stored -- [`App`] has no status/error field yet (can come
    /// later). Deliberately leaves [`App::file_view`]/[`App::pane`] alone --
    /// a diff-load failure while the file pane is open in the background
    /// (opened via `d` from [`Pane::File`], say) shouldn't close it.
    LoadFailed(String),
    /// [`Cmd::LoadFile`] failed; closes the file pane gracefully (e.g. a
    /// node with no files, or a read error) rather than leaving `pane` on
    /// [`Pane::File`] with nothing loaded. The message is not stored, same
    /// as [`Msg::LoadFailed`].
    FileLoadFailed(String),
    /// `t`: flip [`App::show_tests`], recompute `layers` from
    /// [`App::visible_graph`], and re-seat focus if it landed on a node that
    /// just got hidden. Only acted on on [`Screen::Graph`] with no picker
    /// open, matching every other graph-view message.
    ToggleTests,
    /// `Enter` on [`Pane::Graph`]: open the file viewer pane for the focused
    /// node, switching `pane` to [`Pane::File`] and emitting
    /// [`Cmd::LoadFile`]. Only acted on on [`Screen::Graph`]/[`Pane::Graph`]
    /// with no picker open.
    OpenFile,
    /// [`Cmd::LoadFile`] succeeded: store the loaded state. Fired both by
    /// the initial [`Msg::OpenFile`] and by the live-preview reload that
    /// follows focus while the pane is open (see [`Msg::FocusMove`]).
    FileLoaded(FileViewState),
    /// `Esc` on [`Pane::File`]: close the file viewer pane and return
    /// keyboard focus to [`Pane::Graph`].
    CloseFile,
    /// `j`/`k` on [`Pane::File`]: scroll the current file by `delta` rows,
    /// clamped. A no-op with no file pane open.
    FileScroll(i32),
    /// `Ctrl-d`/`Ctrl-u` on [`Pane::File`]: scroll by half of
    /// [`App::viewport_rows`] rows in the direction of `delta` (`1`/`-1`).
    /// A no-op with no file pane open.
    FileHalfPage(i32),
    /// `gg` on [`Pane::File`]: jump to the top of the current file.
    FileJumpTop,
    /// `G` on [`Pane::File`]: jump to the bottom of the current file.
    FileJumpBottom,
    /// `]c` on [`Pane::File`]: jump to the next changed range in the
    /// current file.
    FileNextChange,
    /// `[c` on [`Pane::File`]: jump to the previous changed range in the
    /// current file.
    FilePrevChange,
    /// `]f` on [`Pane::File`]: switch to the next file backing the node,
    /// clamped.
    FileNextFile,
    /// `[f` on [`Pane::File`]: switch to the previous file backing the
    /// node, clamped.
    FilePrevFile,
    /// `Ctrl-w h`: move keyboard focus to [`Pane::Graph`].
    PaneLeft,
    /// `Ctrl-w l`: move keyboard focus to [`Pane::File`]. A no-op if no
    /// file pane is open.
    PaneRight,
    /// `c` on [`Pane::Graph`]: capture an "architecture" review comment
    /// anchored to the focused node as a whole (as opposed to a line-range
    /// comment captured via `:VdiffComment` inside the embedded nvim pane).
    /// Only acted on on [`Screen::Graph`]/[`Pane::Graph`] with no picker
    /// open, matching every other graph-pane message. Emits
    /// [`Cmd::CommentNode`] -- everything past "which node" (opening its
    /// first backing file, prompting for text, whether nvim mode is even
    /// active to prompt with) is glue-side IO `core` has no business
    /// knowing about.
    CommentNode,
    /// `gt` on [`Pane::Graph`]: open the focused module's matched test
    /// module's file, per [`crate::graph::test_modules::matched_test_module`].
    /// Only acted on on [`Screen::Graph`]/[`Pane::Graph`] with no picker
    /// open, matching every other graph-pane message. `focus` does not
    /// move -- the test node is pruned from the visible layout regardless
    /// of [`App::show_tests`], so there's nothing in `layers` to focus it
    /// onto; the module stays focused while its test's file shows in
    /// [`Pane::File`].
    GoToTest,
    /// `v` on [`Pane::Graph`]: toggle the focused node's reviewed flag (see
    /// [`App::reviewed`]). Only acted on on [`Screen::Graph`]/[`Pane::Graph`]
    /// with no picker open, matching every other graph-pane message; a
    /// further no-op (via [`review::toggle_reviewed`] itself) if the focused
    /// node isn't [`crate::core::review::is_changed`] -- only changed nodes
    /// are markable. Emits [`Cmd::PersistReviewState`] exactly when the
    /// toggle actually did something, so a `v` on an unchanged node (or off
    /// the graph pane) never triggers a save.
    ToggleReviewed,
    /// `j`/`k` on the `--tui` rail view: move focus up/down the fold-aware
    /// visible row list (see [`crate::core::rail_view::visible_rows`]),
    /// rather than the layer/x-center-nearest logic [`Msg::FocusMove`] uses
    /// for the GUI's layer-grid navigation -- see `crate::tui::mod`'s doc
    /// for why the rail view needs a distinct message instead of reusing
    /// `FocusMove` with changed semantics (in short: `map_key`/`FocusMove`
    /// are shared with the GUI, which must keep its own h/j/k/l behavior
    /// unchanged). Only acted on on [`Screen::Graph`]/[`Pane::Graph`] with
    /// no picker open, matching every other graph-pane message. A no-op at
    /// either end of the row list, or if `focus` isn't currently in
    /// `visible_rows` at all (shouldn't happen, but defended rather than
    /// panicking).
    RailFocusMove(RailDirection),
    /// `h` on the `--tui` rail view: collapse the namespace immediately
    /// containing the focused row (see
    /// [`crate::core::rail_view::RailRow`]'s doc). If `focus` already names
    /// a *collapsed* namespace row, this collapses that namespace's own
    /// parent instead -- both cases resolve identically, since collapsing
    /// works off `App.graph.node(focus).parent` regardless of whether
    /// `focus` is a plain drawn node or a previously-collapsed namespace
    /// id, letting repeated `h` zoom out one namespace layer at a time. A
    /// no-op if the focused row has no parent namespace to collapse into
    /// (it's already top-level). Only acted on on
    /// [`Screen::Graph`]/[`Pane::Graph`] with no picker open. Focus re-seats
    /// onto the resulting collapsed row. Also drops any already-collapsed
    /// *descendant* of the newly-collapsed parent from
    /// [`App::fold_collapsed`] -- those entries are now redundant (their
    /// namespace is absorbed into the parent's own row) and, left behind,
    /// would round-trip with [`Msg::ExpandFocusedNamespace`]'s one-level
    /// semantics into a stale nested fold that no longer corresponds to
    /// anything reachable from the visible row list.
    CollapseFocusedNamespace,
    /// `l` on the `--tui` rail view: expand the namespace `focus` currently
    /// names by exactly one level -- a no-op unless `focus` is actually a
    /// collapsed row (i.e. present in [`App::fold_collapsed`]). Removes
    /// `focus` from [`App::fold_collapsed`] (revealing its immediate
    /// children), then re-collapses every one of those children that is
    /// itself a namespace (has children of its own) -- so a namespace's
    /// direct leaf modules become visible alongside its child namespaces as
    /// single collapsed rows, while grandchildren stay hidden until that
    /// child namespace is itself expanded. This is what keeps a single `l`
    /// on a namespace with dozens of nested descendants from exploding the
    /// whole subtree into view at once (the previous behavior, which simply
    /// removed `focus` from `fold_collapsed` with nothing re-inserted).
    /// Repeated `l` on the newly-revealed child namespace rows zooms in one
    /// further level at a time. Focus re-seats onto the first now-visible
    /// row that was inside the expanded namespace (see
    /// [`crate::core::rail_view::first_visible_descendant`], which -- given
    /// the one-level re-collapse above -- lands on either a direct leaf
    /// child or a newly-collapsed child-namespace row, never a
    /// grandchild), mirroring the GUI's `toggle_tests` re-seat precedent for
    /// a fold operation that can drop the currently focused row out of
    /// existence. Only acted on on [`Screen::Graph`]/[`Pane::Graph`] with no
    /// picker open.
    ExpandFocusedNamespace,
}

/// I/O the caller should perform as a result of [`update`]. `update` never
/// performs I/O itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Nothing to do.
    None,
    /// Load diff state for the given node, reporting back via
    /// [`Msg::DiffLoaded`]/[`Msg::LoadFailed`].
    LoadDiff(NodeId),
    /// Load file-viewer state for the given node, reporting back via
    /// [`Msg::FileLoaded`]/[`Msg::LoadFailed`]. Emitted by [`Msg::OpenFile`]
    /// and, while the file pane is already open, by any message that moves
    /// `focus` (live preview -- see [`Msg::FocusMove`]).
    LoadFile(NodeId),
    /// `App::layers` changed shape (currently only [`Msg::ToggleTests`]) --
    /// the caller must rebuild its [`crate::graph::layout::LayoutResult`]
    /// from [`App::visible_graph`] before painting again.
    Relayout,
    /// [`Msg::CommentNode`]: capture an architecture comment for the given
    /// node. Glue's job: in nvim mode, open the node's first backing file
    /// (same as [`Cmd::LoadFile`]) and trigger the comment-compose flow
    /// prefilled with line range 1..1 and this node id attached; outside
    /// nvim mode, there's no text-input surface to compose with, so glue
    /// just logs a one-time stderr note instead.
    CommentNode(NodeId),
    /// [`Msg::ToggleReviewed`] actually toggled something: the glue should
    /// re-derive the on-disk review state from [`App::graph`]/
    /// [`App::reviewed`] and save it (see
    /// [`crate::review::review_state::capture`]/
    /// [`crate::review::store::save_review_state`]). Fired on every
    /// successful toggle rather than batched -- crash-safety (an
    /// interrupted review resumes from the last mark) matters more here
    /// than avoiding a few extra small writes.
    PersistReviewState,
}

/// Advance `app` in response to `msg`, returning the new state and any
/// command the caller should execute. Pure: performs no I/O.
///
/// Wraps [`update_inner`] (the actual per-`Msg` reducer) with one final
/// pass: whenever [`App::fold_collapsed`] is non-empty, `App::focus` is
/// remapped through [`rail_view::effective_row_id`] so it always names
/// something [`rail_view::visible_rows`] actually has a row for. This is
/// the fix for a real bug found in review: `update_inner`'s individual
/// focus-setting arms (`Msg::FollowDeps`/`FollowDependents`'s direct jump,
/// `Msg::PickerSelect`, `Msg::FocusSet`, `Msg::ToggleTests`'s re-seat) each
/// set `App::focus` to a *drawn* node id straight out of `App::layers`,
/// with no idea whether that node currently sits inside a collapsed
/// namespace -- `App::layers`/`App::fold_collapsed` are orthogonal, so a
/// drawn id can be perfectly valid there while having no corresponding row
/// in the rail view at all. Left unfixed, that desyncs `App::focus` from
/// the visible row list: the rail view's scroll-into-view math, the
/// focused-row highlight, and the focused-edge accent coloring (see
/// `crate::tui::render`) all key off "which row is `App::focus`", and
/// `Msg::RailFocusMove`'s own `position()` lookup (see that handler) comes
/// up empty and goes dead -- effectively soft-locking `j`/`k` until enough
/// `Msg::CollapseFocusedNamespace` calls happen to climb focus back out on
/// its own. Applying the remap once, centrally, after every single
/// dispatch is simpler and harder to miss than threading it into each
/// individual arm by hand, and it's provably a no-op for the GUI: that
/// frontend never populates `fold_collapsed` (nothing there ever collapses
/// anything), so [`rail_view::effective_row_id`] short-circuits to identity
/// on every call it makes.
pub fn update(app: App, msg: Msg) -> (App, Cmd) {
    let (mut app, cmd) = update_inner(app, msg);
    if !app.fold_collapsed.is_empty() {
        app.focus = rail_view::effective_row_id(&app.graph, &app.focus, &app.fold_collapsed);
    }
    // Central backstop for the invariant every individual focus-setting arm
    // above is supposed to maintain on its own: `App::focus` must satisfy
    // `is_drawn(&focus) || fold_collapsed.contains(&focus)`, or hjkl and
    // friends silently and permanently soft-lock (see `App::visible_graph`'s
    // doc for the bug class -- three prior fixes, plus a fourth found and
    // fixed alongside this backstop in `follow`, all broke this same
    // invariant by deriving a focus/navigation candidate from `app.graph`,
    // the raw never-test-pruned graph, instead of `app.visible_graph()`/
    // `app.layers`). That history is the case for a structural guard here
    // rather than trusting the next call site to remember the pattern:
    // `debug_assert!` fails loudly in dev/test builds (where
    // `debug_assertions` is on by default, including under `cargo test`)
    // the moment a new arm reintroduces the bug, rather than waiting for
    // someone to notice hjkl is dead in a manual pass. Release builds
    // compile the `debug_assert!` away entirely (its own documented
    // behavior), so `repair_stray_focus` always runs there instead,
    // trading a silent focus jump for keeping a real user's session
    // navigable rather than permanently locked. Run unconditionally, not
    // gated on `!fold_collapsed.is_empty()` the way the remap above it is:
    // `is_drawn` alone decides the outcome once `fold_collapsed` is empty
    // (always true on the GUI path), and an `O(layers)` scan once per
    // dispatch is negligible next to the layout pass a `Cmd::Relayout`
    // already re-triggers.
    //
    // Both halves skip entirely when `app.layers` is empty: an empty
    // `layers` means there is genuinely nothing drawn to focus at all (a
    // wholly empty change set, or -- see `toggle_tests`'s doc -- every node
    // just got test-hidden), and `main::build_initial_app` already seeds
    // `focus` with the sentinel `NodeId("")` for exactly that case. That's
    // a legitimate, self-healing blank state, not a stray focus to flag or
    // repair -- `reseat_focus` treats an empty `new_layers` the same way,
    // leaving `focus` untouched rather than inventing a target that
    // doesn't exist.
    if !app.layers.is_empty() {
        debug_assert!(
            app.is_drawn(&app.focus) || app.fold_collapsed.contains(&app.focus),
            "App::focus {:?} is neither drawn nor a collapsed namespace after a dispatch -- \
             a focus-setting arm derived a candidate from app.graph (raw) instead of \
             app.visible_graph()/app.layers; see App::visible_graph's doc for the bug class",
            app.focus,
        );
        repair_stray_focus(&mut app);
    }
    (app, cmd)
}

/// Release-build half of [`update`]'s central invariant backstop (see that
/// function's doc): if `app.focus` satisfies neither `is_drawn` nor
/// `fold_collapsed.contains`, reseat it onto the first row of the first
/// layer -- the same deterministic fallback [`reseat_focus`] falls back to
/// when it can't find a better answer. Only called when `app.layers` is
/// non-empty (see the call site's own doc), so `app.layers.first()` always
/// has a row to offer.
fn repair_stray_focus(app: &mut App) {
    if app.is_drawn(&app.focus) || app.fold_collapsed.contains(&app.focus) {
        return;
    }
    if let Some(first) = app.layers.first().and_then(|layer| layer.first()) {
        app.focus = first.clone();
    }
}

/// The per-`Msg` reducer [`update`] wraps with the fold-aware focus remap
/// described on that function's doc. Split out only so that remap is
/// applied exactly once, in exactly one place, regardless of which arm
/// below actually changed `focus`.
fn update_inner(mut app: App, msg: Msg) -> (App, Cmd) {
    match msg {
        Msg::FocusMove(dir) => {
            if !on_graph_with_no_picker_and_graph_pane(&app) {
                return (app, Cmd::None);
            }
            let old_focus = app.focus.clone();
            app.focus = move_focus(&app.layers, &app.rows, &app.focus, dir);
            let cmd = reload_file_on_focus_change(&app, &old_focus);
            (app, cmd)
        }
        Msg::FocusSet(id) => {
            let acceptable = app.is_drawn(&id) || app.fold_collapsed.contains(&id);
            if !on_graph_with_no_picker_and_graph_pane(&app) || !acceptable {
                return (app, Cmd::None);
            }
            let old_focus = app.focus.clone();
            app.focus = id;
            let cmd = reload_file_on_focus_change(&app, &old_focus);
            (app, cmd)
        }
        Msg::FollowDeps => follow(app, dep_targets),
        Msg::FollowDependents => follow(app, dependent_sources),
        Msg::PickerMove(delta) => {
            picker_move(&mut app, delta);
            (app, Cmd::None)
        }
        Msg::PickerSelect => {
            let old_focus = app.focus.clone();
            picker_select(&mut app);
            let cmd = reload_file_on_focus_change(&app, &old_focus);
            (app, cmd)
        }
        Msg::PickerCancel => {
            app.picker = None;
            (app, Cmd::None)
        }
        Msg::OpenDiff => open_diff(app),
        Msg::CloseDiff => {
            app.screen = Screen::Graph;
            app.diff = None;
            (app, Cmd::None)
        }
        Msg::DiffLoaded(state) => {
            app.diff = Some(state);
            (app, Cmd::None)
        }
        Msg::LoadFailed(_message) => {
            app.screen = Screen::Graph;
            (app, Cmd::None)
        }
        Msg::FileLoadFailed(_message) => {
            app.file_view = None;
            app.pane = Pane::Graph;
            (app, Cmd::None)
        }
        Msg::DiffScroll(delta) => {
            with_diff_pane(&mut app, |diff| diff.scroll(delta));
            (app, Cmd::None)
        }
        Msg::DiffNextHunk => {
            with_diff_pane(&mut app, DiffPaneState::next_hunk);
            (app, Cmd::None)
        }
        Msg::DiffPrevHunk => {
            with_diff_pane(&mut app, DiffPaneState::prev_hunk);
            (app, Cmd::None)
        }
        Msg::DiffToggleMode => {
            with_diff_pane(&mut app, DiffPaneState::toggle_mode);
            (app, Cmd::None)
        }
        Msg::DiffNextFile => {
            with_diff_pane(&mut app, |diff| diff.shift_file(1));
            (app, Cmd::None)
        }
        Msg::DiffPrevFile => {
            with_diff_pane(&mut app, |diff| diff.shift_file(-1));
            (app, Cmd::None)
        }
        Msg::ToggleTests => toggle_tests(app),
        Msg::OpenFile => open_file(app),
        Msg::FileLoaded(state) => {
            app.file_view = Some(state);
            (app, Cmd::None)
        }
        Msg::CloseFile => {
            app.file_view = None;
            app.pane = Pane::Graph;
            (app, Cmd::None)
        }
        Msg::FileScroll(delta) => {
            with_file_view(&mut app, |fv| {
                let max = fv.total_rows().saturating_sub(1);
                fv.scroll(delta, max);
            });
            (app, Cmd::None)
        }
        Msg::FileHalfPage(direction) => {
            let half = (app.viewport_rows / 2).max(1) as i32;
            with_file_view(&mut app, |fv| {
                let max = fv.total_rows().saturating_sub(1);
                fv.scroll(direction * half, max);
            });
            (app, Cmd::None)
        }
        Msg::FileJumpTop => {
            with_file_view(&mut app, FileViewState::jump_top);
            (app, Cmd::None)
        }
        Msg::FileJumpBottom => {
            with_file_view(&mut app, |fv| {
                let total = fv.total_rows();
                fv.jump_bottom(total);
            });
            (app, Cmd::None)
        }
        Msg::FileNextChange => {
            with_file_view(&mut app, FileViewState::next_change);
            (app, Cmd::None)
        }
        Msg::FilePrevChange => {
            with_file_view(&mut app, FileViewState::prev_change);
            (app, Cmd::None)
        }
        Msg::FileNextFile => {
            with_file_view(&mut app, |fv| fv.shift_file(1));
            (app, Cmd::None)
        }
        Msg::FilePrevFile => {
            with_file_view(&mut app, |fv| fv.shift_file(-1));
            (app, Cmd::None)
        }
        Msg::PaneLeft => {
            app.pane = Pane::Graph;
            (app, Cmd::None)
        }
        Msg::PaneRight => {
            if app.file_view.is_some() {
                app.pane = Pane::File;
            }
            (app, Cmd::None)
        }
        Msg::CommentNode => {
            if !on_graph_with_no_picker_and_graph_pane(&app) {
                return (app, Cmd::None);
            }
            let focus = app.focus.clone();
            (app, Cmd::CommentNode(focus))
        }
        Msg::GoToTest => go_to_test(app),
        Msg::ToggleReviewed => toggle_reviewed(app),
        Msg::RailFocusMove(dir) => rail_focus_move(app, dir),
        Msg::CollapseFocusedNamespace => collapse_focused_namespace(app),
        Msg::ExpandFocusedNamespace => expand_focused_namespace(app),
    }
}

/// Handle [`Msg::RailFocusMove`]: step `focus` to the previous/next entry
/// in [`rail_view::visible_rows`]'s flattened list. See that message's own
/// doc for why this doesn't reuse [`move_focus`].
fn rail_focus_move(mut app: App, dir: RailDirection) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    let rows = rail_view::visible_rows(&app.graph, &app.layers, &app.fold_collapsed);
    let Some(pos) = rows.iter().position(|row| row.id() == &app.focus) else {
        return (app, Cmd::None);
    };
    let new_pos = match dir {
        RailDirection::Up => pos.checked_sub(1),
        RailDirection::Down => {
            let next = pos + 1;
            (next < rows.len()).then_some(next)
        }
    };
    let Some(new_pos) = new_pos else {
        return (app, Cmd::None);
    };
    let old_focus = app.focus.clone();
    app.focus = rows[new_pos].id().clone();
    let cmd = reload_file_on_focus_change(&app, &old_focus);
    (app, cmd)
}

/// Handle [`Msg::CollapseFocusedNamespace`]. See that message's own doc.
fn collapse_focused_namespace(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    let Some(parent) = app.graph.node(&app.focus).and_then(|n| n.parent.clone()) else {
        return (app, Cmd::None);
    };
    app.fold_collapsed.insert(parent.clone());
    // Prune any already-collapsed descendant of `parent` -- see
    // `Msg::CollapseFocusedNamespace`'s doc for why a stale nested entry
    // left behind here would round-trip badly with the one-level expand.
    let graph = &app.graph;
    let parent_for_prune = parent.clone();
    app.fold_collapsed
        .retain(|id| id == &parent_for_prune || !is_descendant_of(graph, id, &parent_for_prune));
    app.focus = parent;
    (app, Cmd::None)
}

/// Handle [`Msg::ExpandFocusedNamespace`]. See that message's own doc.
fn expand_focused_namespace(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    if !app.fold_collapsed.remove(&app.focus) {
        return (app, Cmd::None);
    }
    let namespace = app.focus.clone();
    // Walk `App::visible_graph()`, not `app.graph` -- `app.graph` is the
    // raw, never-test-pruned graph (see its own doc), so a name-first child
    // that's actually a hidden test module (`App::show_tests == false`,
    // the default) would still be found there and picked as the re-collapse/
    // reseat target below. That id then has no row anywhere: it's absent
    // from `App::layers` (`is_drawn` false -- `assign_layers` runs over the
    // test-hidden graph) and isn't in `fold_collapsed` either, which is
    // exactly the shape every hjkl/rail-move path treats as "not found" --
    // a silent, permanent focus lockout (found in review: expanding a
    // namespace whose first child alphabetically is a test module loses the
    // cursor entirely, recoverable only by re-collapsing). `visible_graph()`
    // has already pruned those children out of `children`/`node` lookups
    // entirely, so this walk can only ever land on something `is_drawn`/
    // `fold_collapsed` actually accepts.
    let visible = app.visible_graph();
    // Reveal exactly one level: re-collapse every immediate child that is
    // itself a namespace (has children of its own), so `namespace`'s direct
    // leaf modules become visible while grandchildren stay hidden behind a
    // freshly-collapsed child-namespace row. See `Msg::ExpandFocusedNamespace`'s
    // doc for why a flat "reveal everything" expand is unusable on a dense
    // change set.
    if let Some(children) = visible.node(&namespace).map(|n| n.children.clone()) {
        for child in children {
            if visible.node(&child).is_some_and(|c| !c.children.is_empty()) {
                app.fold_collapsed.insert(child);
            }
        }
    }
    if let Some(reseated) =
        rail_view::first_visible_descendant(&visible, &namespace, &app.fold_collapsed)
    {
        app.focus = reseated;
    }
    (app, Cmd::None)
}

/// `true` if `id` sits anywhere under `ancestor` in the parent chain (`id`
/// itself doesn't count). Used by [`collapse_focused_namespace`] to prune
/// now-redundant nested fold entries once a namespace collapses one of its
/// own ancestors.
fn is_descendant_of(graph: &ProjectGraph, id: &NodeId, ancestor: &NodeId) -> bool {
    let mut current = graph.node(id).and_then(|n| n.parent.clone());
    while let Some(p) = current {
        if &p == ancestor {
            return true;
        }
        current = graph.node(&p).and_then(|n| n.parent.clone());
    }
    false
}

/// Handle [`Msg::ToggleReviewed`]: only on [`Screen::Graph`]/[`Pane::Graph`]
/// with no picker open, delegate to [`review::toggle_reviewed`] for the
/// focused node and emit [`Cmd::PersistReviewState`] iff `reviewed` actually
/// changed (so a `v` on an unchanged node -- a no-op inside
/// `toggle_reviewed` itself -- never triggers a save).
fn toggle_reviewed(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    let focus = app.focus.clone();
    let before = app.reviewed.clone();
    review::toggle_reviewed(&mut app.reviewed, &app.graph, &focus);
    if app.reviewed == before {
        return (app, Cmd::None);
    }
    (app, Cmd::PersistReviewState)
}

/// Handle [`Msg::ToggleTests`]: flip `show_tests`, recompute `layers`/`rows`
/// from a full [`crate::graph::layout::layout`] pass over
/// [`App::visible_graph`] (rather than calling `assign_layers` separately --
/// this is also what keeps `layers`/`rows` from drifting out of sync with
/// each other, since they're now two views of the same [`LayoutResult`]),
/// and re-seat focus (see [`reseat_focus`]) if it's no longer drawn.
fn toggle_tests(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    app.show_tests = !app.show_tests;
    let old_layers = std::mem::take(&mut app.layers);
    let result = layout(&app.visible_graph());
    app.rows = rows_with_x_centers(&result);
    app.layers = result.layers;
    if !app.is_drawn(&app.focus) {
        app.focus = reseat_focus(&old_layers, &app.layers, &app.focus);
    }
    (app, Cmd::Relayout)
}

/// Find `id`'s `(layer_idx, pos_idx)` in `layers`, or `None` if absent.
fn locate(layers: &[Vec<NodeId>], id: &NodeId) -> Option<(usize, usize)> {
    for (layer_idx, row) in layers.iter().enumerate() {
        if let Some(pos_idx) = row.iter().position(|n| n == id) {
            return Some((layer_idx, pos_idx));
        }
    }
    None
}

/// Pick a new focus after `focus` was hidden by a `layers` rebuild: land on
/// the node at the same `(layer_idx, pos_idx)` in `new_layers` (both
/// clamped to bounds) it held in `old_layers`, or `new_layers[0][0]` if
/// `focus` wasn't found in `old_layers` at all or `new_layers` is empty.
fn reseat_focus(old_layers: &[Vec<NodeId>], new_layers: &[Vec<NodeId>], focus: &NodeId) -> NodeId {
    if new_layers.is_empty() {
        return focus.clone();
    }
    let Some((old_layer_idx, old_pos_idx)) = locate(old_layers, focus) else {
        return new_layers[0][0].clone();
    };
    let layer_idx = old_layer_idx.min(new_layers.len() - 1);
    let row = &new_layers[layer_idx];
    if row.is_empty() {
        return new_layers[0][0].clone();
    }
    let pos_idx = old_pos_idx.min(row.len() - 1);
    row[pos_idx].clone()
}

/// Shared guard for the `Diff*` messages that mutate the open diff pane:
/// only on [`Screen::Diff`] with a diff pane loaded. A no-op otherwise.
fn with_diff_pane(app: &mut App, f: impl FnOnce(&mut DiffPaneState)) {
    if app.screen != Screen::Diff {
        return;
    }
    if let Some(diff) = app.diff.as_mut() {
        f(diff);
    }
}

/// Whether `app` is in the state [`Msg::OpenDiff`] requires: on
/// [`Screen::Graph`], with no picker overlay open. Not pane-gated -- `d`
/// opens the full-screen diff from either [`Pane::Graph`] or [`Pane::File`].
fn on_graph_with_no_picker(app: &App) -> bool {
    app.screen == Screen::Graph && app.picker.is_none()
}

/// Whether `app` is in the state [`Msg::FocusMove`]/[`Msg::FocusSet`]/
/// [`Msg::FollowDeps`]/[`Msg::FollowDependents`]/[`Msg::OpenFile`]/
/// [`Msg::ToggleTests`] require: [`on_graph_with_no_picker`], plus keyboard
/// focus on [`Pane::Graph`] -- these all act on graph navigation state that
/// only [`Pane::Graph`]'s keymap bindings ever reach.
fn on_graph_with_no_picker_and_graph_pane(app: &App) -> bool {
    on_graph_with_no_picker(app) && app.pane == Pane::Graph
}

/// If `app.focus` differs from `old_focus` and the file pane is open,
/// reload it for the new focus -- the "live preview" behavior described on
/// [`Cmd::LoadFile`]. `Cmd::None` otherwise.
fn reload_file_on_focus_change(app: &App, old_focus: &NodeId) -> Cmd {
    if app.file_view.is_some() && &app.focus != old_focus {
        Cmd::LoadFile(app.focus.clone())
    } else {
        Cmd::None
    }
}

/// Shared handler for [`Msg::FollowDeps`]/[`Msg::FollowDependents`]: look up
/// `candidates` via `edges_fn`, then no-op/jump/open-picker per how many
/// there are.
///
/// `edges_fn` is called over [`App::visible_graph`], not `app.graph` --
/// `app.graph` is the raw, never-test-pruned graph (see its own doc), and
/// `dep_targets`/`dependent_sources` (`crate::core::focus`) walk *every*
/// edge in whatever graph they're given, with no notion of `show_tests`.
/// The common real-world shape this guards against: a leaf module with no
/// production consumers at all, only its own test file depending on it
/// (`test -> module`, an ordinary import edge) -- `dependent_sources` over
/// the raw graph would surface that test module as `focus`'s sole
/// candidate, and the `candidates.len() == 1` arm below jumps straight to
/// it with no `is_drawn`/`fold_collapsed` check (unlike `Msg::FocusSet`,
/// which guards explicitly). With `show_tests` off (the default), that test
/// id has no row anywhere -- absent from `layers` (test-pruned) and never
/// in `fold_collapsed` (nothing collapsed it) -- silently and permanently
/// soft-locking `hjkl` (found in review: this is the same bug class fixed
/// three times already for other call sites -- see `App::visible_graph`'s
/// doc). `visible_graph()` has already pruned test edges out entirely (see
/// `crate::graph::filter::prune`), so every candidate `edges_fn` can return
/// here is guaranteed to be `is_drawn`.
fn follow(mut app: App, edges_fn: impl Fn(&ProjectGraph, &NodeId) -> Vec<NodeId>) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    let old_focus = app.focus.clone();
    let candidates = edges_fn(&app.visible_graph(), &app.focus);
    let mut cmd = Cmd::None;
    match candidates.len() {
        0 => {}
        1 => {
            app.focus = candidates.into_iter().next().expect("checked len == 1");
            cmd = reload_file_on_focus_change(&app, &old_focus);
        }
        _ => {
            app.picker = Some(EdgePicker {
                candidates,
                selected: 0,
            })
        }
    }
    (app, cmd)
}

/// Handle [`Msg::PickerMove`]: shift the open picker's selection by `delta`,
/// clamped to its candidate list's bounds. A no-op if no picker is open.
fn picker_move(app: &mut App, delta: i32) {
    let Some(picker) = &mut app.picker else {
        return;
    };
    if picker.candidates.is_empty() {
        return;
    }
    let new_selected = picker.selected as i32 + delta;
    let max = picker.candidates.len() as i32 - 1;
    picker.selected = new_selected.clamp(0, max) as usize;
}

/// Handle [`Msg::PickerSelect`]: focus the open picker's highlighted
/// candidate and close it. A no-op if no picker is open.
fn picker_select(app: &mut App) {
    let Some(picker) = app.picker.take() else {
        return;
    };
    if let Some(id) = picker.candidates.get(picker.selected) {
        app.focus = id.clone();
    }
}

/// Whether `id` has at least one backing file -- the guard [`open_diff`]/
/// [`open_file`] apply before emitting [`Cmd::LoadDiff`]/[`Cmd::LoadFile`]
/// for it. Unreachable on the GUI in practice: the GUI never focuses a
/// file-less node at all. [`Msg::FocusSet`]'s guard does also accept ids
/// currently in `App::fold_collapsed` (see that variant's own doc), but the
/// GUI never populates `fold_collapsed` (nothing there ever collapses
/// anything), so that half of the guard is dead weight for it -- the only
/// ids it can ever pass are ones `App::is_drawn` accepts, i.e. present in
/// `App::layers`, and [`crate::graph::layers::assign_layers`] excludes
/// every file-less synthetic namespace container from `layers` in the
/// first place -- see that module's doc. The `--tui` rail view is
/// what actually needs this: a collapsed namespace row's id (see
/// [`crate::core::rail_view::RailRow::Collapsed`]) is exactly such a
/// file-less container, and it *is* focusable there (that's the whole
/// point of fold-by-namespace), so `Enter`/`d` on one would otherwise emit
/// a load command for a node with nothing to load.
fn has_files(graph: &ProjectGraph, id: &NodeId) -> bool {
    graph.node(id).is_some_and(|node| !node.files.is_empty())
}

/// Handle [`Msg::OpenDiff`]: only on [`Screen::Graph`] with no picker open,
/// switch to [`Screen::Diff`] and emit [`Cmd::LoadDiff`] for the node whose
/// file is actually shown: [`App::file_view`]'s node when [`Pane::File`] is
/// open (after [`Msg::GoToTest`], that's the test, not `focus`, which stays
/// on the module -- see [`Msg::GoToTest`]'s doc), `focus` otherwise. A
/// no-op (screen/pane untouched) if that target has no files at all -- see
/// [`has_files`]'s doc for why this guard exists and why it's a no-op on
/// the GUI path.
fn open_diff(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker(&app) {
        return (app, Cmd::None);
    }
    let target = match (&app.file_view, app.pane) {
        (Some(file_view), Pane::File) => file_view.node.clone(),
        _ => app.focus.clone(),
    };
    if !has_files(&app.graph, &target) {
        return (app, Cmd::None);
    }
    app.screen = Screen::Diff;
    (app, Cmd::LoadDiff(target))
}

/// Shared guard for the `File*` messages that mutate the open file pane:
/// only on [`Screen::Graph`] with a file pane loaded. A no-op otherwise.
fn with_file_view(app: &mut App, f: impl FnOnce(&mut FileViewState)) {
    if app.screen != Screen::Graph {
        return;
    }
    if let Some(file_view) = app.file_view.as_mut() {
        f(file_view);
    }
}

/// Handle [`Msg::OpenFile`]: only on [`Screen::Graph`]/[`Pane::Graph`] with
/// no picker open, switch keyboard focus to [`Pane::File`] and emit
/// [`Cmd::LoadFile`] for the focused node. `pane` flips before the load
/// completes (see [`App::file_view`]'s doc) rather than waiting for
/// [`Msg::FileLoaded`] -- the caller's `Cmd::LoadFile` executor runs
/// synchronously within the same dispatch, so there's no visible gap. A
/// no-op (pane untouched) if the focused node has no files at all -- see
/// [`has_files`]'s doc for why this guard exists and why it's a no-op on
/// the GUI path.
fn open_file(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    let focus = app.focus.clone();
    if !has_files(&app.graph, &focus) {
        return (app, Cmd::None);
    }
    app.pane = Pane::File;
    (app, Cmd::LoadFile(focus))
}

/// Handle [`Msg::GoToTest`]: only on [`Screen::Graph`]/[`Pane::Graph`] with
/// no picker open, look up the focused node's matched test module and, if
/// there is one, switch `pane` to [`Pane::File`] and emit [`Cmd::LoadFile`]
/// for the test's id -- but leave `focus` on the module itself (see
/// [`Msg::GoToTest`]'s doc). A no-op if there's no matching test.
fn go_to_test(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    let Some(test_id) = matched_test_module(&app.graph, &app.focus) else {
        return (app, Cmd::None);
    };
    app.pane = Pane::File;
    (app, Cmd::LoadFile(test_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepEdge, DepKind, GitStatus, ModuleNode};
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// `leaf_a`/`leaf_b` are children of the synthetic (no-files) node
    /// `root`; `target_x/y/z` sit alongside them with no hierarchy relevant
    /// to these tests. Edges: `leaf_a -> target_x`, `leaf_b -> target_x`,
    /// `leaf_b -> target_y`, `leaf_b -> target_z`. So `dep_targets(leaf_a)`
    /// is a single hit (`target_x`), `dep_targets(leaf_b)` is three
    /// (`target_x/y/z`), `dep_targets(target_x)` is zero.
    /// `dependent_sources(target_x)` is two (`leaf_a`, `leaf_b`),
    /// `dependent_sources(target_y)` is a single hit (`leaf_b`),
    /// `dependent_sources(root)` is zero. Every drawn node has one edge into
    /// `target_x/y/z`, so layering (see [`crate::graph::layers`]) puts
    /// `[leaf_a, leaf_b]` at layer 0 and `[target_x, target_y, target_z]` at
    /// layer 1 -- `root` is synthetic (no files) and never appears in a
    /// layer at all.
    fn graph_fixture() -> ProjectGraph {
        let root = NodeId::from("root");
        let leaf_a = NodeId::from("leaf_a");
        let leaf_b = NodeId::from("leaf_b");
        let target_x = NodeId::from("target_x");
        let target_y = NodeId::from("target_y");
        let target_z = NodeId::from("target_z");

        let leaf = |id: &NodeId, name: &str, parent: Option<NodeId>| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent,
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![crate::graph::model::FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };

        let mut nodes = HashMap::new();
        nodes.insert(
            root.clone(),
            ModuleNode {
                id: root.clone(),
                display_name: "root".to_string(),
                parent: None,
                children: vec![leaf_a.clone(), leaf_b.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(leaf_a.clone(), leaf(&leaf_a, "leaf_a", Some(root.clone())));
        nodes.insert(leaf_b.clone(), leaf(&leaf_b, "leaf_b", Some(root.clone())));
        nodes.insert(target_x.clone(), leaf(&target_x, "target_x", None));
        nodes.insert(target_y.clone(), leaf(&target_y, "target_y", None));
        nodes.insert(target_z.clone(), leaf(&target_z, "target_z", None));

        let edges = vec![
            DepEdge {
                from: leaf_a.clone(),
                to: target_x.clone(),
                kind: DepKind::Use,
            },
            DepEdge {
                from: leaf_b.clone(),
                to: target_x.clone(),
                kind: DepKind::Import,
            },
            DepEdge {
                from: leaf_b.clone(),
                to: target_y.clone(),
                kind: DepKind::Alias,
            },
            DepEdge {
                from: leaf_b,
                to: target_z.clone(),
                kind: DepKind::Require,
            },
        ];

        ProjectGraph {
            roots: vec![root, target_x, target_y, target_z],
            nodes,
            edges,
        }
    }

    fn app_at(focus: &str) -> App {
        let graph = graph_fixture();
        let result = crate::graph::layout::layout(&graph);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        App {
            graph,
            layers: result.layers,
            rows,
            focus: NodeId::from(focus),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        }
    }

    #[test]
    fn focus_move_updates_focus_on_graph_with_no_picker() {
        // Layer 0 is [leaf_a, leaf_b] (see `graph_fixture`'s docs): Right
        // moves within that row.
        let app = app_at("leaf_a");
        let (app, cmd) = update(app, Msg::FocusMove(Direction::Right));
        assert_eq!(app.focus, NodeId::from("leaf_b"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn focus_move_noop_when_picker_open() {
        let mut app = app_at("leaf_a");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x")],
            selected: 0,
        });
        let (app, _) = update(app, Msg::FocusMove(Direction::Right));
        assert_eq!(app.focus, NodeId::from("leaf_a"));
    }

    #[test]
    fn focus_move_noop_off_graph_screen() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        let (app, _) = update(app, Msg::FocusMove(Direction::Right));
        assert_eq!(app.focus, NodeId::from("leaf_a"));
    }

    #[test]
    fn focus_set_updates_focus_for_known_drawn_node() {
        let app = app_at("leaf_a");
        let (app, cmd) = update(app, Msg::FocusSet(NodeId::from("target_y")));
        assert_eq!(app.focus, NodeId::from("target_y"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn focus_set_noop_for_unknown_node() {
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::FocusSet(NodeId::from("nonexistent")));
        assert_eq!(app.focus, NodeId::from("leaf_a"));
    }

    #[test]
    fn focus_set_noop_for_synthetic_node() {
        // `root` has no files -- it's excluded from `layers` entirely and
        // must be rejected as a focus target even though it's a real node
        // in the graph.
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::FocusSet(NodeId::from("root")));
        assert_eq!(app.focus, NodeId::from("leaf_a"));
    }

    #[test]
    fn focus_set_can_land_on_a_collapsed_namespace_row() {
        // Reproduces the plane/canvas hjkl soft-lock found in review:
        // collapse `root` (focus lands on it, same as `zc` would leave it),
        // move focus away onto an unrelated sibling with no relation to
        // `root` (standing in for the hjkl step that walks off it), then
        // dispatch `FocusSet` back onto `root`'s own id -- exactly what
        // `plane_key_msg`/`canvas_key_msg` compute via `move_focus` over
        // `plane_focus_grid`/`canvas_focus_grid` (both of which legitimately
        // include collapsed-namespace ids as focus targets) once hjkl points
        // back at the collapsed row. Before the fix, `is_drawn(&root)` is
        // false (`root` has no files and `assign_layers` excludes it -- see
        // `focus_set_noop_for_synthetic_node`), so the guard swallowed the
        // move silently and permanently: `root` is the *only* way back to
        // its own children, so a `move_focus` that returns it becomes an
        // unrecoverable dead end.
        let mut app = app_at("leaf_a");
        app.fold_collapsed.insert(NodeId::from("root"));
        app.focus = NodeId::from("target_x");
        let (app, cmd) = update(app, Msg::FocusSet(NodeId::from("root")));
        assert_eq!(app.focus, NodeId::from("root"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn follow_deps_zero_candidates_is_noop() {
        let app = app_at("target_x");
        let (app, cmd) = update(app, Msg::FollowDeps);
        assert_eq!(app.focus, NodeId::from("target_x"));
        assert!(app.picker.is_none());
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn follow_deps_one_candidate_jumps_directly() {
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::FollowDeps);
        assert_eq!(app.focus, NodeId::from("target_x"));
        assert!(app.picker.is_none());
    }

    #[test]
    fn follow_deps_many_candidates_opens_picker() {
        let app = app_at("leaf_b");
        let (app, _) = update(app, Msg::FollowDeps);
        assert_eq!(app.focus, NodeId::from("leaf_b"), "focus unchanged so far");
        let picker = app.picker.expect("picker should open");
        assert_eq!(
            picker.candidates,
            vec![
                NodeId::from("target_x"),
                NodeId::from("target_y"),
                NodeId::from("target_z"),
            ]
        );
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn follow_dependents_zero_candidates_is_noop() {
        // `leaf_a` has no incoming edges (see `graph_fixture`'s doc) and,
        // unlike `root`, is an ordinary drawn node -- `App::focus` must
        // always satisfy `is_drawn`/`fold_collapsed` (see `update`'s central
        // backstop), so a synthetic file-less node like `root` is never a
        // legitimate focus value to begin with.
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::FollowDependents);
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert!(app.picker.is_none());
    }

    #[test]
    fn follow_dependents_one_candidate_jumps_directly() {
        let app = app_at("target_y");
        let (app, _) = update(app, Msg::FollowDependents);
        assert_eq!(app.focus, NodeId::from("leaf_b"));
    }

    #[test]
    fn follow_dependents_many_candidates_opens_picker() {
        let app = app_at("target_x");
        let (app, _) = update(app, Msg::FollowDependents);
        let picker = app.picker.expect("picker should open");
        assert_eq!(
            picker.candidates,
            vec![NodeId::from("leaf_a"), NodeId::from("leaf_b")]
        );
    }

    #[test]
    fn follow_deps_noop_when_picker_already_open() {
        let mut app = app_at("leaf_a");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x")],
            selected: 0,
        });
        let (app, _) = update(app, Msg::FollowDeps);
        assert_eq!(
            app.focus,
            NodeId::from("leaf_a"),
            "no jump while picker open"
        );
    }

    /// Reproduces the fourth instance of the recurring focus-lockout bug
    /// class (see `App::visible_graph`'s doc): `follow` (backing
    /// `Msg::FollowDeps`/`Msg::FollowDependents`) computed its candidates via
    /// `dep_targets`/`dependent_sources` over `app.graph` -- the raw,
    /// never-test-pruned graph -- rather than `app.visible_graph()`. A
    /// module whose *only* dependent is its own test file (a common shape:
    /// nothing in production code depends on a leaf module, only its test
    /// does) would surface that test module's id as the single candidate,
    /// and the `candidates.len() == 1` arm jumps straight to it with no
    /// `is_drawn`/`fold_collapsed` check at all (unlike `Msg::FocusSet`,
    /// which guards explicitly). With `show_tests` off (the default), the
    /// test module has no row anywhere -- not in `layers` (test-pruned) and
    /// not in `fold_collapsed` (nothing collapsed it) -- so this is bug
    /// shape (b) from the class doc: total, silent cursor loss, since the
    /// central fold-aware remap in `update` only fires when
    /// `fold_collapsed` is non-empty, which it never is here.
    #[test]
    fn follow_dependents_does_not_jump_onto_a_hidden_test_module() {
        let leaf_a = NodeId::from("leaf_a");
        let leaf_a_test = NodeId::from("leaf_a_test");

        let module = |id: &NodeId, name: &str| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![crate::graph::model::FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };

        let mut nodes = HashMap::new();
        nodes.insert(leaf_a.clone(), module(&leaf_a, "leaf_a"));
        // Ends with "Test", satisfying `is_test_module` -- the only thing
        // that "depends on" `leaf_a` in this graph is its own test.
        nodes.insert(leaf_a_test.clone(), module(&leaf_a_test, "leaf_aTest"));

        let graph = ProjectGraph {
            roots: vec![leaf_a.clone(), leaf_a_test.clone()],
            nodes,
            edges: vec![DepEdge {
                from: leaf_a_test.clone(),
                to: leaf_a.clone(),
                kind: DepKind::Use,
            }],
        };

        // Build `layers`/`rows` the way `main::build_initial_app` actually
        // does: from the test-hidden graph, not the raw one -- `leaf_a_test`
        // must never appear in either.
        let visible = hide_test_modules(&graph).0;
        let visible_layout = crate::graph::layout::layout(&visible);
        let rows = crate::graph::layout::rows_with_x_centers(&visible_layout);

        let app = App {
            graph,
            layers: visible_layout.layers,
            rows,
            focus: leaf_a.clone(),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        };

        let (app, _) = update(app, Msg::FollowDependents);

        assert_eq!(
            app.focus, leaf_a,
            "focus must not jump onto a test module hidden from `layers`; \
             got {:?}, which is neither drawn nor a collapsed namespace, \
             permanently soft-locking hjkl",
            app.focus
        );
    }

    #[test]
    fn picker_move_clamps_to_bounds() {
        let mut app = app_at("leaf_b");
        app.picker = Some(EdgePicker {
            candidates: vec![
                NodeId::from("target_x"),
                NodeId::from("target_y"),
                NodeId::from("target_z"),
            ],
            selected: 0,
        });
        let (app, _) = update(app, Msg::PickerMove(-1));
        assert_eq!(app.picker.as_ref().unwrap().selected, 0, "clamped at 0");

        let (app, _) = update(app, Msg::PickerMove(1));
        assert_eq!(app.picker.as_ref().unwrap().selected, 1);

        let (app, _) = update(app, Msg::PickerMove(5));
        assert_eq!(app.picker.as_ref().unwrap().selected, 2, "clamped at max");
    }

    #[test]
    fn picker_move_noop_when_no_picker() {
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::PickerMove(1));
        assert!(app.picker.is_none());
    }

    #[test]
    fn picker_select_sets_focus_and_closes_picker() {
        let mut app = app_at("leaf_b");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x"), NodeId::from("target_y")],
            selected: 1,
        });
        let (app, cmd) = update(app, Msg::PickerSelect);
        assert_eq!(app.focus, NodeId::from("target_y"));
        assert!(app.picker.is_none());
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn picker_select_noop_when_no_picker() {
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::PickerSelect);
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert!(app.picker.is_none());
    }

    #[test]
    fn picker_cancel_closes_without_changing_focus() {
        let mut app = app_at("leaf_b");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x"), NodeId::from("target_y")],
            selected: 1,
        });
        let (app, _) = update(app, Msg::PickerCancel);
        assert_eq!(app.focus, NodeId::from("leaf_b"));
        assert!(app.picker.is_none());
    }

    #[test]
    fn open_diff_switches_screen_and_emits_load_diff() {
        let app = app_at("leaf_a");
        let (app, cmd) = update(app, Msg::OpenDiff);
        assert_eq!(app.screen, Screen::Diff);
        assert_eq!(cmd, Cmd::LoadDiff(NodeId::from("leaf_a")));
    }

    #[test]
    fn open_diff_noop_when_picker_open() {
        let mut app = app_at("leaf_a");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x")],
            selected: 0,
        });
        let (app, cmd) = update(app, Msg::OpenDiff);
        assert_eq!(app.screen, Screen::Graph);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn open_diff_works_from_file_pane_too() {
        let mut app = app_at("leaf_a");
        app.pane = Pane::File;
        let (app, cmd) = update(app, Msg::OpenDiff);
        assert_eq!(app.screen, Screen::Diff);
        assert_eq!(cmd, Cmd::LoadDiff(NodeId::from("leaf_a")));
    }

    #[test]
    fn open_diff_targets_the_displayed_files_node_after_go_to_test() {
        // After `gt`, `focus` stays on the module while `file_view.node` is
        // the test -- `d` should diff what's actually on screen (the test).
        let mut app = app_at_with_test_module("module");
        app.pane = Pane::File;
        app.file_view = Some(FileViewState::new(NodeId::from("module_test"), vec![]));
        let (app, cmd) = update(app, Msg::OpenDiff);
        assert_eq!(app.focus, NodeId::from("module"));
        assert_eq!(cmd, Cmd::LoadDiff(NodeId::from("module_test")));
    }

    #[test]
    fn open_diff_targets_focus_from_graph_pane_even_with_a_stale_file_view() {
        let mut app = app_at_with_test_module("module");
        app.file_view = Some(FileViewState::new(NodeId::from("module_test"), vec![]));
        let (_app, cmd) = update(app, Msg::OpenDiff);
        assert_eq!(cmd, Cmd::LoadDiff(NodeId::from("module")));
    }

    /// An empty diff pane (no files loaded) for the given node -- enough
    /// for tests that only care about screen/pane-presence guards.
    fn empty_diff_pane(node: &str) -> DiffPaneState {
        DiffPaneState::new(NodeId::from(node), vec![])
    }

    /// A diff pane with one file of two single-line hunks, for tests that
    /// exercise scroll/hunk-jump/file-switch transitions.
    fn loaded_diff_pane(node: &str) -> DiffPaneState {
        use crate::core::diff_state::FileEntry;
        use crate::diffing::hunks::{DiffHunk, FileDiff, LinePair};

        let hunk = |base: u32| DiffHunk {
            lines: vec![LinePair::Unchanged { base, head: base }],
        };
        DiffPaneState::new(
            NodeId::from(node),
            vec![FileEntry {
                path: PathBuf::from("f.rs"),
                diff: FileDiff {
                    hunks: vec![hunk(0), hunk(1)],
                    base_lines: vec!["a".to_string(), "b".to_string()],
                    head_lines: vec!["a".to_string(), "b".to_string()],
                },
            }],
        )
    }

    #[test]
    fn close_diff_returns_to_graph_and_clears_diff_preserving_focus() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        app.diff = Some(empty_diff_pane("leaf_a"));
        let (app, cmd) = update(app, Msg::CloseDiff);
        assert_eq!(app.screen, Screen::Graph);
        assert!(app.diff.is_none());
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn diff_loaded_stores_diff_state() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        let (app, _) = update(app, Msg::DiffLoaded(empty_diff_pane("leaf_a")));
        assert_eq!(app.diff, Some(empty_diff_pane("leaf_a")));
    }

    #[test]
    fn load_failed_returns_to_graph_screen() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        let (app, cmd) = update(app, Msg::LoadFailed("boom".to_string()));
        assert_eq!(app.screen, Screen::Graph);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn load_failed_leaves_file_pane_alone() {
        // A diff-load failure (e.g. `d` from Pane::File while the file pane
        // is open in the background) shouldn't close the unrelated file
        // pane -- only `Msg::FileLoadFailed` does that.
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        app.pane = Pane::File;
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, cmd) = update(app, Msg::LoadFailed("boom".to_string()));
        assert_eq!(app.screen, Screen::Graph);
        assert_eq!(app.pane, Pane::File);
        assert!(app.file_view.is_some());
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn diff_scroll_moves_row_on_diff_screen() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        app.diff = Some(loaded_diff_pane("leaf_a"));
        let (app, cmd) = update(app, Msg::DiffScroll(1));
        assert_eq!(app.diff.unwrap().scroll_row, 1);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn diff_scroll_noop_on_graph_screen() {
        let mut app = app_at("leaf_a");
        app.diff = Some(loaded_diff_pane("leaf_a"));
        let (app, _) = update(app, Msg::DiffScroll(1));
        assert_eq!(app.diff.unwrap().scroll_row, 0, "no-op off Diff screen");
    }

    #[test]
    fn diff_scroll_noop_with_no_pane_loaded() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        app.diff = None;
        let (app, _) = update(app, Msg::DiffScroll(1));
        assert!(app.diff.is_none());
    }

    #[test]
    fn diff_next_hunk_and_prev_hunk_jump_scroll_row() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        app.diff = Some(loaded_diff_pane("leaf_a"));
        let (app, _) = update(app, Msg::DiffNextHunk);
        assert_eq!(app.diff.as_ref().unwrap().scroll_row, 1);
        let (app, _) = update(app, Msg::DiffPrevHunk);
        assert_eq!(app.diff.unwrap().scroll_row, 0);
    }

    #[test]
    fn diff_toggle_mode_flips_mode() {
        use crate::core::diff_state::DiffMode;
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        app.diff = Some(loaded_diff_pane("leaf_a"));
        let (app, _) = update(app, Msg::DiffToggleMode);
        assert_eq!(app.diff.unwrap().mode, DiffMode::Unified);
    }

    #[test]
    fn diff_next_file_and_prev_file_clamp() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        app.diff = Some(loaded_diff_pane("leaf_a"));
        let (app, _) = update(app, Msg::DiffNextFile);
        assert_eq!(app.diff.as_ref().unwrap().file_index, 0, "clamped: 1 file");
        let (app, _) = update(app, Msg::DiffPrevFile);
        assert_eq!(app.diff.unwrap().file_index, 0);
    }

    /// `graph_fixture` plus one isolated test module (`test_x`, files under
    /// `test/`, no edges) -- lands in its own trailing layer when shown,
    /// disappears entirely when hidden.
    fn graph_fixture_with_test_node() -> ProjectGraph {
        let mut g = graph_fixture();
        let test_id = NodeId::from("test_x");
        g.nodes.insert(
            test_id.clone(),
            ModuleNode {
                id: test_id.clone(),
                display_name: "TestX".to_string(),
                parent: None,
                children: vec![],
                status: GitStatus::Unchanged,
                files: vec![crate::graph::model::FileRef {
                    path: PathBuf::from("test/test_x_test.exs"),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        );
        g.roots.push(test_id);
        g
    }

    /// `graph_fixture` plus one test module matched to `leaf_a` (parented
    /// under it, so they share a top-level root, and named `leaf_aTest` so
    /// stripping the `Test` suffix yields `leaf_a`'s own display name) --
    /// exercises `visible_graph`'s grouping branch: once `show_tests` is on,
    /// this node should be pruned from the drawn graph (see
    /// `group_matched_test_modules`) rather than rendered standalone.
    fn graph_fixture_with_matched_test_node() -> ProjectGraph {
        let mut g = graph_fixture();
        let test_id = NodeId::from("leaf_a_test");
        g.nodes.insert(
            test_id.clone(),
            ModuleNode {
                id: test_id.clone(),
                display_name: "leaf_aTest".to_string(),
                parent: Some(NodeId::from("leaf_a")),
                children: vec![],
                status: GitStatus::Modified,
                files: vec![crate::graph::model::FileRef {
                    path: PathBuf::from("test/leaf_a_test.rs"),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        );
        g.roots.push(test_id);
        g
    }

    #[test]
    fn visible_graph_groups_a_matched_test_module_into_its_tested_node_when_tests_are_shown() {
        let g = graph_fixture_with_matched_test_node();
        let mut app = app_at("leaf_a");
        app.graph = g;
        app.show_tests = true;

        let visible = app.visible_graph();

        assert!(
            visible.node(&NodeId::from("leaf_a")).is_some(),
            "tested node stays drawn"
        );
        assert!(
            visible.node(&NodeId::from("leaf_a_test")).is_none(),
            "matched test module is grouped into leaf_a's box, not drawn standalone"
        );
    }

    #[test]
    fn visible_graph_hides_a_matched_test_module_entirely_when_tests_are_off() {
        let g = graph_fixture_with_matched_test_node();
        let mut app = app_at("leaf_a");
        app.graph = g;
        app.show_tests = false;

        let visible = app.visible_graph();

        assert!(visible.node(&NodeId::from("leaf_a")).is_some());
        assert!(visible.node(&NodeId::from("leaf_a_test")).is_none());
    }

    /// A graph made up entirely of test modules (one standalone node, name
    /// ending `Test` so [`crate::graph::test_modules::is_test_module`]
    /// matches) -- exercises [`initial_show_tests`]'s blank-canvas case,
    /// where hiding tests would prune every node.
    fn all_test_graph() -> ProjectGraph {
        let id = NodeId::from("only_test");
        let mut nodes = HashMap::new();
        nodes.insert(
            id.clone(),
            ModuleNode {
                id: id.clone(),
                display_name: "OnlyTest".to_string(),
                parent: None,
                children: vec![],
                status: GitStatus::Modified,
                files: vec![crate::graph::model::FileRef {
                    path: PathBuf::from("only_test.rs"),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        );
        ProjectGraph {
            roots: vec![id],
            nodes,
            edges: vec![],
        }
    }

    #[test]
    fn initial_show_tests_false_when_graph_has_no_test_modules() {
        assert!(!initial_show_tests(&graph_fixture()));
    }

    #[test]
    fn initial_show_tests_false_when_hiding_tests_still_leaves_nodes() {
        assert!(!initial_show_tests(&graph_fixture_with_test_node()));
    }

    #[test]
    fn initial_show_tests_true_when_hiding_tests_would_blank_the_graph() {
        assert!(initial_show_tests(&all_test_graph()));
    }

    #[test]
    fn initial_show_tests_false_for_an_already_empty_graph() {
        let empty = ProjectGraph {
            roots: vec![],
            nodes: HashMap::new(),
            edges: vec![],
        };
        assert!(!initial_show_tests(&empty));
    }

    #[test]
    fn toggle_tests_reveals_hidden_test_node_and_recomputes_layers() {
        let g = graph_fixture_with_test_node();
        let visible = crate::graph::test_modules::hide_test_modules(&g).0;
        let result = crate::graph::layout::layout(&visible);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        let app = App {
            graph: g,
            layers: result.layers,
            rows,
            focus: NodeId::from("leaf_a"),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        };
        assert!(!app
            .layers
            .iter()
            .flatten()
            .any(|id| id == &NodeId::from("test_x")));

        let (app, cmd) = update(app, Msg::ToggleTests);

        assert!(app.show_tests);
        assert_eq!(cmd, Cmd::Relayout);
        assert!(
            app.layers
                .iter()
                .flatten()
                .any(|id| id == &NodeId::from("test_x")),
            "test_x should now be drawn"
        );
        assert_eq!(app.focus, NodeId::from("leaf_a"), "focus unaffected");
    }

    #[test]
    fn toggle_tests_reseats_focus_when_the_focused_node_becomes_hidden() {
        let g = graph_fixture_with_test_node();
        let result = crate::graph::layout::layout(&g);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        let app = App {
            layers: result.layers,
            rows,
            graph: g,
            focus: NodeId::from("test_x"),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: true,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        };

        let (app, cmd) = update(app, Msg::ToggleTests);

        assert!(!app.show_tests);
        assert_eq!(cmd, Cmd::Relayout);
        assert_ne!(app.focus, NodeId::from("test_x"));
        assert!(
            app.layers.iter().flatten().any(|id| id == &app.focus),
            "reseated focus must be a drawn node"
        );
    }

    #[test]
    fn toggle_tests_noop_when_picker_open() {
        let mut app = app_at("leaf_a");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x")],
            selected: 0,
        });
        let (app, cmd) = update(app, Msg::ToggleTests);
        assert!(!app.show_tests);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn toggle_tests_noop_off_graph_screen() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        let (app, cmd) = update(app, Msg::ToggleTests);
        assert!(!app.show_tests);
        assert_eq!(cmd, Cmd::None);
    }

    /// An empty file view (no files loaded) for the given node -- enough
    /// for tests that only care about screen/pane-presence guards.
    fn empty_file_view(node: &str) -> FileViewState {
        FileViewState::new(NodeId::from(node), vec![])
    }

    /// A file view with one file of 5 lines, changed range `[2, 3]`, for
    /// tests that exercise scroll/jump/change-nav/file-switch transitions.
    fn loaded_file_view(node: &str) -> FileViewState {
        use crate::core::file_view::FileViewEntry;

        FileViewState::new(
            NodeId::from(node),
            vec![FileViewEntry {
                path: PathBuf::from("f.rs"),
                lines: vec!["a", "b", "c", "d", "e"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                changed_ranges: vec![(2, 3)],
                deleted: false,
            }],
        )
    }

    #[test]
    fn open_file_switches_pane_and_emits_load_file() {
        let app = app_at("leaf_a");
        let (app, cmd) = update(app, Msg::OpenFile);
        assert_eq!(app.pane, Pane::File);
        assert_eq!(cmd, Cmd::LoadFile(NodeId::from("leaf_a")));
    }

    #[test]
    fn open_file_noop_when_picker_open() {
        let mut app = app_at("leaf_a");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x")],
            selected: 0,
        });
        let (app, cmd) = update(app, Msg::OpenFile);
        assert_eq!(app.pane, Pane::Graph);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn open_file_noop_when_already_on_file_pane() {
        let mut app = app_at("leaf_a");
        app.pane = Pane::File;
        let (_app, cmd) = update(app, Msg::OpenFile);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn comment_node_emits_cmd_for_focused_node() {
        let app = app_at("leaf_a");
        let (app, cmd) = update(app, Msg::CommentNode);
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert_eq!(cmd, Cmd::CommentNode(NodeId::from("leaf_a")));
    }

    #[test]
    fn comment_node_noop_when_picker_open() {
        let mut app = app_at("leaf_a");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x")],
            selected: 0,
        });
        let (_app, cmd) = update(app, Msg::CommentNode);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn comment_node_noop_when_on_file_pane() {
        let mut app = app_at("leaf_a");
        app.pane = Pane::File;
        let (_app, cmd) = update(app, Msg::CommentNode);
        assert_eq!(cmd, Cmd::None);
    }

    /// A minimal graph with one module (`module`, display name `Foo`) and
    /// its matched test module (`module_test`, display name `FooTest`,
    /// sharing `module`'s parent so [`crate::graph::test_modules::tested_node_id`]'s
    /// same-root check passes) -- for [`Msg::GoToTest`] tests, which don't
    /// need `graph_fixture`'s dependency-edge shape.
    fn graph_fixture_with_test_module() -> ProjectGraph {
        let root = NodeId::from("root");
        let module = NodeId::from("module");
        let test = NodeId::from("module_test");

        let leaf = |id: &NodeId, name: &str| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: Some(root.clone()),
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![crate::graph::model::FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };

        let mut nodes = HashMap::new();
        nodes.insert(
            root.clone(),
            ModuleNode {
                id: root.clone(),
                display_name: "root".to_string(),
                parent: None,
                children: vec![module.clone(), test.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(module.clone(), leaf(&module, "Foo"));
        nodes.insert(test.clone(), leaf(&test, "FooTest"));

        ProjectGraph {
            roots: vec![root],
            nodes,
            edges: vec![],
        }
    }

    fn app_at_with_test_module(focus: &str) -> App {
        let graph = graph_fixture_with_test_module();
        let result = crate::graph::layout::layout(&graph);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        App {
            graph,
            layers: result.layers,
            rows,
            focus: NodeId::from(focus),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        }
    }

    #[test]
    fn go_to_test_switches_pane_and_emits_load_file_for_the_test() {
        let app = app_at_with_test_module("module");
        let (app, cmd) = update(app, Msg::GoToTest);
        assert_eq!(app.pane, Pane::File);
        assert_eq!(app.focus, NodeId::from("module"));
        assert_eq!(cmd, Cmd::LoadFile(NodeId::from("module_test")));
    }

    #[test]
    fn go_to_test_noop_when_no_matching_test() {
        let app = app_at("leaf_a");
        let (app, cmd) = update(app, Msg::GoToTest);
        assert_eq!(app.pane, Pane::Graph);
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn go_to_test_noop_when_picker_open() {
        let mut app = app_at_with_test_module("module");
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("module_test")],
            selected: 0,
        });
        let (app, cmd) = update(app, Msg::GoToTest);
        assert_eq!(app.pane, Pane::Graph);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn comment_node_noop_on_diff_screen() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        let (_app, cmd) = update(app, Msg::CommentNode);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn file_loaded_stores_file_view() {
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::FileLoaded(empty_file_view("leaf_a")));
        assert_eq!(app.file_view, Some(empty_file_view("leaf_a")));
    }

    #[test]
    fn close_file_clears_file_view_and_resets_pane() {
        let mut app = app_at("leaf_a");
        app.pane = Pane::File;
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, cmd) = update(app, Msg::CloseFile);
        assert!(app.file_view.is_none());
        assert_eq!(app.pane, Pane::Graph);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn file_scroll_moves_row_with_file_view_open() {
        let mut app = app_at("leaf_a");
        app.file_view = Some(loaded_file_view("leaf_a"));
        let (app, cmd) = update(app, Msg::FileScroll(1));
        assert_eq!(app.file_view.unwrap().scroll_row, 1);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn file_scroll_noop_with_no_file_view() {
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::FileScroll(1));
        assert!(app.file_view.is_none());
    }

    #[test]
    fn file_scroll_noop_off_graph_screen() {
        let mut app = app_at("leaf_a");
        app.screen = Screen::Diff;
        app.file_view = Some(loaded_file_view("leaf_a"));
        let (app, _) = update(app, Msg::FileScroll(1));
        assert_eq!(
            app.file_view.unwrap().scroll_row,
            0,
            "no-op off Graph screen"
        );
    }

    #[test]
    fn file_half_page_scrolls_by_half_viewport_rows() {
        let mut app = app_at("leaf_a");
        app.file_view = Some(loaded_file_view("leaf_a"));
        app.viewport_rows = 4; // half = 2
        let (app, _) = update(app, Msg::FileHalfPage(1));
        assert_eq!(app.file_view.as_ref().unwrap().scroll_row, 2);
        let (app, _) = update(app, Msg::FileHalfPage(-1));
        assert_eq!(app.file_view.unwrap().scroll_row, 0);
    }

    #[test]
    fn file_half_page_uses_at_least_one_row_when_viewport_rows_is_small() {
        let mut app = app_at("leaf_a");
        app.file_view = Some(loaded_file_view("leaf_a"));
        app.viewport_rows = 1; // half = max(0, 1) = 1
        let (app, _) = update(app, Msg::FileHalfPage(1));
        assert_eq!(app.file_view.unwrap().scroll_row, 1);
    }

    #[test]
    fn file_jump_top_and_bottom() {
        let mut app = app_at("leaf_a");
        app.file_view = Some(loaded_file_view("leaf_a"));
        app.file_view.as_mut().unwrap().scroll_row = 2;
        let (app, _) = update(app, Msg::FileJumpTop);
        assert_eq!(app.file_view.as_ref().unwrap().scroll_row, 0);
        let (app, _) = update(app, Msg::FileJumpBottom);
        assert_eq!(app.file_view.unwrap().scroll_row, 4);
    }

    #[test]
    fn file_next_change_and_prev_change() {
        let mut app = app_at("leaf_a");
        app.file_view = Some(loaded_file_view("leaf_a"));
        let (app, _) = update(app, Msg::FileNextChange);
        assert_eq!(app.file_view.as_ref().unwrap().scroll_row, 2);
        // Already at the only range's start -- `[c` has nothing earlier to
        // jump to, so it's a no-op (matches `FileViewState::prev_change`'s
        // "no wrap" contract, exercised directly in `core::file_view`).
        let (app, _) = update(app, Msg::FilePrevChange);
        assert_eq!(app.file_view.unwrap().scroll_row, 2);
    }

    #[test]
    fn file_next_file_and_prev_file_clamp() {
        let mut app = app_at("leaf_a");
        app.file_view = Some(loaded_file_view("leaf_a"));
        let (app, _) = update(app, Msg::FileNextFile);
        assert_eq!(
            app.file_view.as_ref().unwrap().file_index,
            0,
            "clamped: 1 file"
        );
        let (app, _) = update(app, Msg::FilePrevFile);
        assert_eq!(app.file_view.unwrap().file_index, 0);
    }

    #[test]
    fn pane_right_noop_with_no_file_view() {
        let app = app_at("leaf_a");
        let (app, _) = update(app, Msg::PaneRight);
        assert_eq!(app.pane, Pane::Graph);
    }

    #[test]
    fn pane_right_switches_pane_when_file_view_open() {
        let mut app = app_at("leaf_a");
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, _) = update(app, Msg::PaneRight);
        assert_eq!(app.pane, Pane::File);
    }

    #[test]
    fn pane_left_switches_pane_back_to_graph() {
        let mut app = app_at("leaf_a");
        app.pane = Pane::File;
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, _) = update(app, Msg::PaneLeft);
        assert_eq!(app.pane, Pane::Graph);
    }

    #[test]
    fn file_load_failed_closes_file_pane_and_resets_pane() {
        let mut app = app_at("leaf_a");
        app.pane = Pane::File;
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, cmd) = update(app, Msg::FileLoadFailed("boom".to_string()));
        assert!(app.file_view.is_none());
        assert_eq!(app.pane, Pane::Graph);
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn focus_move_reloads_file_when_file_view_open() {
        // Layer 0 is [leaf_a, leaf_b]: Right moves within that row.
        let mut app = app_at("leaf_a");
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, cmd) = update(app, Msg::FocusMove(Direction::Right));
        assert_eq!(app.focus, NodeId::from("leaf_b"));
        assert_eq!(cmd, Cmd::LoadFile(NodeId::from("leaf_b")));
    }

    #[test]
    fn focus_move_no_reload_when_no_file_view_open() {
        let app = app_at("leaf_a");
        let (app, cmd) = update(app, Msg::FocusMove(Direction::Right));
        assert_eq!(app.focus, NodeId::from("leaf_b"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn focus_move_no_reload_when_focus_unchanged() {
        // `target_x` is alone at the end of layer 1 -- moving Right is a
        // no-op, so even with a file pane open there's nothing to reload.
        let mut app = app_at("target_z");
        app.file_view = Some(empty_file_view("target_z"));
        let (app, cmd) = update(app, Msg::FocusMove(Direction::Right));
        assert_eq!(app.focus, NodeId::from("target_z"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn focus_move_noop_when_pane_is_file() {
        // Graph navigation only acts on Pane::Graph -- the keymap never
        // sends FocusMove from Pane::File, but the guard defends it too.
        let mut app = app_at("leaf_a");
        app.pane = Pane::File;
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, cmd) = update(app, Msg::FocusMove(Direction::Right));
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn follow_deps_reloads_file_when_file_view_open() {
        let mut app = app_at("leaf_a");
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, cmd) = update(app, Msg::FollowDeps);
        assert_eq!(app.focus, NodeId::from("target_x"));
        assert_eq!(cmd, Cmd::LoadFile(NodeId::from("target_x")));
    }

    #[test]
    fn picker_select_reloads_file_when_file_view_open() {
        let mut app = app_at("leaf_b");
        app.file_view = Some(empty_file_view("leaf_b"));
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x"), NodeId::from("target_y")],
            selected: 1,
        });
        let (app, cmd) = update(app, Msg::PickerSelect);
        assert_eq!(app.focus, NodeId::from("target_y"));
        assert_eq!(cmd, Cmd::LoadFile(NodeId::from("target_y")));
    }

    /// `leaf_a`/`leaf_b` share a synthetic namespace parent (`root`),
    /// `target_x/y/z` are top-level (no namespace to collapse into) --
    /// enough to exercise `CollapseFocusedNamespace`/`ExpandFocusedNamespace`
    /// against both a node with a collapsible parent and one without.
    fn app_for_fold() -> App {
        app_at("leaf_a")
    }

    #[test]
    fn collapse_focused_namespace_collapses_the_parent_and_reseats_focus() {
        let app = app_for_fold();
        let (app, cmd) = update(app, Msg::CollapseFocusedNamespace);
        assert_eq!(app.focus, NodeId::from("root"));
        assert!(app.fold_collapsed.contains(&NodeId::from("root")));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn collapse_focused_namespace_noop_with_no_parent_to_collapse_into() {
        let app = app_at("target_x");
        let (app, _) = update(app, Msg::CollapseFocusedNamespace);
        assert_eq!(app.focus, NodeId::from("target_x"));
        assert!(app.fold_collapsed.is_empty());
    }

    #[test]
    fn collapse_focused_namespace_noop_when_picker_open() {
        let mut app = app_for_fold();
        app.picker = Some(EdgePicker {
            candidates: vec![NodeId::from("target_x")],
            selected: 0,
        });
        let (app, _) = update(app, Msg::CollapseFocusedNamespace);
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert!(app.fold_collapsed.is_empty());
    }

    #[test]
    fn collapse_focused_namespace_on_an_already_collapsed_row_climbs_one_level_further() {
        // `root` itself has no parent (it's top-level), so collapsing again
        // while focus already names the collapsed namespace is a no-op --
        // there's nothing further out to fold into.
        let mut app = app_for_fold();
        app.fold_collapsed.insert(NodeId::from("root"));
        app.focus = NodeId::from("root");
        let (app, _) = update(app, Msg::CollapseFocusedNamespace);
        assert_eq!(app.focus, NodeId::from("root"));
    }

    #[test]
    fn expand_focused_namespace_reseats_onto_the_first_visible_descendant() {
        let mut app = app_for_fold();
        app.fold_collapsed.insert(NodeId::from("root"));
        app.focus = NodeId::from("root");

        let (app, cmd) = update(app, Msg::ExpandFocusedNamespace);

        assert!(!app.fold_collapsed.contains(&NodeId::from("root")));
        assert_eq!(app.focus, NodeId::from("leaf_a"), "name-first child");
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn expand_focused_namespace_noop_when_focus_is_not_collapsed() {
        let app = app_for_fold();
        let (app, _) = update(app, Msg::ExpandFocusedNamespace);
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert!(app.fold_collapsed.is_empty());
    }

    // -- One-level semantic zoom (real-use fix: a single `zo`/`l` on a
    // namespace with dozens of nested descendants used to reveal the
    // entire subtree at once) ------------------------------------------

    /// Three-level nesting: `outer` (namespace, top-level) contains `inner`
    /// (namespace) and `leaf_direct` (leaf); `inner` contains `leaf1`/
    /// `leaf2` (both leaves, no further nesting). Enough to exercise one
    /// level of expand revealing `inner` as a still-collapsed row alongside
    /// `leaf_direct`, and a second expand on `inner` bottoming out at plain
    /// leaves.
    fn graph_fixture_nested_namespaces() -> ProjectGraph {
        let outer = NodeId::from("outer");
        let inner = NodeId::from("inner");
        let leaf_direct = NodeId::from("leaf_direct");
        let leaf1 = NodeId::from("leaf1");
        let leaf2 = NodeId::from("leaf2");

        let leaf = |id: &NodeId, name: &str, parent: NodeId| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: Some(parent),
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![crate::graph::model::FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };

        let mut nodes = HashMap::new();
        nodes.insert(
            outer.clone(),
            ModuleNode {
                id: outer.clone(),
                display_name: "outer".to_string(),
                parent: None,
                children: vec![inner.clone(), leaf_direct.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(
            inner.clone(),
            ModuleNode {
                id: inner.clone(),
                display_name: "inner".to_string(),
                parent: Some(outer.clone()),
                children: vec![leaf1.clone(), leaf2.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(
            leaf_direct.clone(),
            leaf(&leaf_direct, "leaf_direct", outer.clone()),
        );
        nodes.insert(leaf1.clone(), leaf(&leaf1, "leaf1", inner.clone()));
        nodes.insert(leaf2.clone(), leaf(&leaf2, "leaf2", inner.clone()));

        ProjectGraph {
            roots: vec![outer],
            nodes,
            edges: vec![],
        }
    }

    fn app_for_nested_fold(focus: &str) -> App {
        let graph = graph_fixture_nested_namespaces();
        let result = crate::graph::layout::layout(&graph);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        App {
            graph,
            layers: result.layers,
            rows,
            focus: NodeId::from(focus),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        }
    }

    #[test]
    fn expand_focused_namespace_reveals_one_level_keeping_grandchildren_hidden() {
        let mut app = app_for_nested_fold("outer");
        app.fold_collapsed.insert(NodeId::from("outer"));
        app.focus = NodeId::from("outer");

        let (app, cmd) = update(app, Msg::ExpandFocusedNamespace);

        // `outer` itself is expanded; `inner` (a child namespace) is
        // re-collapsed in its place, so `leaf1`/`leaf2` stay hidden.
        assert!(!app.fold_collapsed.contains(&NodeId::from("outer")));
        assert!(app.fold_collapsed.contains(&NodeId::from("inner")));
        assert_eq!(app.fold_collapsed.len(), 1);
        // Name-first among `outer`'s children is `inner` -- focus re-seats
        // onto that now-collapsed row, not past it.
        assert_eq!(app.focus, NodeId::from("inner"));
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn expand_focused_namespace_at_deepest_level_reveals_leaves() {
        // Continue from the previous test's end state: `inner` is now the
        // one collapsed row, with only leaves underneath it -- expanding it
        // must bottom out with nothing left to re-collapse.
        let mut app = app_for_nested_fold("inner");
        app.fold_collapsed.insert(NodeId::from("inner"));

        let (app, cmd) = update(app, Msg::ExpandFocusedNamespace);

        assert!(app.fold_collapsed.is_empty());
        assert_eq!(app.focus, NodeId::from("leaf1"), "name-first leaf child");
        assert_eq!(cmd, Cmd::None);
    }

    /// Reproduces the "cursor lost entirely" bug found in review after
    /// 331621c: `outer` (namespace, collapsed) contains two children,
    /// `AaaTest` (a test module -- `is_test_module` matches on the
    /// `Test`-suffixed display name) and `BReal` (an ordinary leaf).
    /// `AaaTest` sorts first by name, so `first_visible_descendant`'s
    /// name-first walk picks it -- but `App::visible_graph()` (what
    /// `App::show_tests == false`, the default, actually draws) prunes
    /// `AaaTest` out entirely. Before the fix, `expand_focused_namespace`
    /// called `rail_view::first_visible_descendant(&app.graph, ...)` --
    /// `app.graph` is the raw, never-test-pruned graph (see its own doc),
    /// so the walk still finds `AaaTest` there and re-seats `App::focus`
    /// onto it. `AaaTest` then has no row in `App::layers` (`is_drawn`
    /// false, per `assign_layers` over the test-hidden graph) and isn't in
    /// `fold_collapsed` either -- exactly the id shape every hjkl/rail-move
    /// path (`move_focus`, `rail_focus_move`) treats as "not found",
    /// silently no-opping in every direction and leaving no row anywhere
    /// matching `App::focus`, i.e. a total, permanent lockout with no key
    /// able to recover it (the report's only escape was `zc`, which
    /// re-collapses `outer` and thus re-seats focus back onto `outer`
    /// itself via the central fold remap in `update`).
    #[test]
    fn expand_focused_namespace_skips_a_name_first_test_module_child() {
        let outer = NodeId::from("outer");
        let test_child = NodeId::from("aaa_test");
        let real_child = NodeId::from("b_real");

        let mut nodes = HashMap::new();
        nodes.insert(
            outer.clone(),
            ModuleNode {
                id: outer.clone(),
                display_name: "outer".to_string(),
                parent: None,
                children: vec![test_child.clone(), real_child.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(
            test_child.clone(),
            ModuleNode {
                id: test_child.clone(),
                display_name: "AaaTest".to_string(),
                parent: Some(outer.clone()),
                children: vec![],
                status: GitStatus::Unchanged,
                files: vec![crate::graph::model::FileRef {
                    path: PathBuf::from("test/aaa_test.rs"),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        );
        nodes.insert(
            real_child.clone(),
            ModuleNode {
                id: real_child.clone(),
                display_name: "BReal".to_string(),
                parent: Some(outer.clone()),
                children: vec![],
                status: GitStatus::Unchanged,
                files: vec![crate::graph::model::FileRef {
                    path: PathBuf::from("lib/b_real.rs"),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        );
        let graph = ProjectGraph {
            roots: vec![outer.clone()],
            nodes,
            edges: vec![],
        };

        // `show_tests` is false (the default) -- `layers`/`rows` are built
        // from the test-hidden graph, mirroring `build_initial_app`.
        let visible = hide_test_modules(&graph).0;
        let result = crate::graph::layout::layout(&visible);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        let mut app = App {
            graph,
            layers: result.layers,
            rows,
            focus: outer.clone(),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        };
        app.fold_collapsed.insert(outer.clone());

        let (app, _) = update(app, Msg::ExpandFocusedNamespace);

        assert_eq!(
            app.focus,
            NodeId::from("b_real"),
            "must skip the test-pruned name-first child and land on the real one"
        );
        assert!(
            app.is_drawn(&app.focus) || app.fold_collapsed.contains(&app.focus),
            "post-expand focus {:?} has no row anywhere -- hjkl/rail-move is locked out",
            app.focus
        );
    }

    #[test]
    fn expand_then_collapse_round_trips_without_stranding_focus() {
        // zo on `outer` reveals `inner` as a collapsed row and reseats
        // focus there; an immediate zc on that row must climb back out to
        // `outer` cleanly, with `fold_collapsed` left in a state that
        // matches exactly one visible row (`outer`), not a stale nested
        // leftover for `inner`.
        let mut app = app_for_nested_fold("outer");
        app.fold_collapsed.insert(NodeId::from("outer"));

        let (app, _) = update(app, Msg::ExpandFocusedNamespace);
        assert_eq!(app.focus, NodeId::from("inner"));

        let (app, _) = update(app, Msg::CollapseFocusedNamespace);

        assert_eq!(app.focus, NodeId::from("outer"));
        assert_eq!(app.fold_collapsed, HashSet::from([NodeId::from("outer")]));

        // The round-tripped focus must correspond to an actual visible row.
        let rows = rail_view::visible_rows(&app.graph, &app.layers, &app.fold_collapsed);
        assert!(
            rows.iter().any(|row| row.id() == &app.focus),
            "focus must land on a row rail_view::visible_rows actually renders"
        );
    }

    #[test]
    fn rail_focus_move_steps_down_the_visible_row_list() {
        // `layers_fixture`'s visible rows (fully expanded) are exactly
        // `app.layers` flattened: [leaf_a, leaf_b, target_x, target_y,
        // target_z].
        let app = app_for_fold();
        let (app, _) = update(app, Msg::RailFocusMove(RailDirection::Down));
        assert_eq!(app.focus, NodeId::from("leaf_b"));
    }

    #[test]
    fn rail_focus_move_up_from_the_first_row_is_a_noop() {
        let app = app_for_fold();
        let (app, _) = update(app, Msg::RailFocusMove(RailDirection::Up));
        assert_eq!(app.focus, NodeId::from("leaf_a"));
    }

    #[test]
    fn rail_focus_move_down_from_the_last_row_is_a_noop() {
        let app = app_at("target_z");
        let (app, _) = update(app, Msg::RailFocusMove(RailDirection::Down));
        assert_eq!(app.focus, NodeId::from("target_z"));
    }

    #[test]
    fn rail_focus_move_skips_over_a_collapsed_namespace_as_one_step() {
        let mut app = app_for_fold();
        app.fold_collapsed.insert(NodeId::from("root"));
        app.focus = NodeId::from("root");
        let (app, _) = update(app, Msg::RailFocusMove(RailDirection::Down));
        assert_eq!(app.focus, NodeId::from("target_x"));
    }

    #[test]
    fn rail_focus_move_reloads_file_when_file_view_open() {
        let mut app = app_for_fold();
        app.file_view = Some(empty_file_view("leaf_a"));
        let (app, cmd) = update(app, Msg::RailFocusMove(RailDirection::Down));
        assert_eq!(app.focus, NodeId::from("leaf_b"));
        assert_eq!(cmd, Cmd::LoadFile(NodeId::from("leaf_b")));
    }

    #[test]
    fn rail_focus_move_noop_off_graph_pane() {
        let mut app = app_for_fold();
        app.pane = Pane::File;
        let (app, cmd) = update(app, Msg::RailFocusMove(RailDirection::Down));
        assert_eq!(app.focus, NodeId::from("leaf_a"));
        assert_eq!(cmd, Cmd::None);
    }

    // -- Fix: focus escaping the visible row set (review feedback) --------
    //
    // `effective_row_id` was only ever applied when collapsing edges for
    // the rail gutter, never when `App::focus` itself was set. So `gd`/
    // `gr`'s direct jump, the edge picker's selection, and any other
    // focus-setting path could land `App::focus` on a drawn node whose
    // ancestor is collapsed -- a raw id with no corresponding row in
    // `rail_view::visible_rows` at all. `update`'s wrapper now remaps
    // `App::focus` through `rail_view::effective_row_id` after every
    // dispatch; these tests pin that down directly, plus confirm the fix
    // is a no-op with an empty `fold_collapsed` (the GUI's permanent
    // state).

    /// `ns` is a synthetic namespace containing two drawn children
    /// (`inner`/`inner2`); a top-level `outer` node depends on both --
    /// enough to drive `gd` into a picker (two candidates) or, with only
    /// one edge wired up, a direct jump. See `single_dep` fixtures below
    /// for the direct-jump shape.
    fn graph_fixture_with_namespace(edges: Vec<DepEdge>) -> ProjectGraph {
        let ns = NodeId::from("ns");
        let inner = NodeId::from("inner");
        let inner2 = NodeId::from("inner2");
        let outer = NodeId::from("outer");

        let leaf = |id: &NodeId, name: &str, parent: Option<NodeId>| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent,
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![crate::graph::model::FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };

        let mut nodes = HashMap::new();
        nodes.insert(
            ns.clone(),
            ModuleNode {
                id: ns.clone(),
                display_name: "ns".to_string(),
                parent: None,
                children: vec![inner.clone(), inner2.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(inner.clone(), leaf(&inner, "inner", Some(ns.clone())));
        nodes.insert(inner2.clone(), leaf(&inner2, "inner2", Some(ns.clone())));
        nodes.insert(outer.clone(), leaf(&outer, "outer", None));

        ProjectGraph {
            roots: vec![ns, outer],
            nodes,
            edges,
        }
    }

    fn edge(from: &str, to: &str) -> DepEdge {
        DepEdge {
            from: NodeId::from(from),
            to: NodeId::from(to),
            kind: DepKind::Use,
        }
    }

    fn app_with_graph(graph: ProjectGraph, focus: &str) -> App {
        let result = crate::graph::layout::layout(&graph);
        let rows = crate::graph::layout::rows_with_x_centers(&result);
        App {
            graph,
            layers: result.layers,
            rows,
            focus: NodeId::from(focus),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
            reviewed: HashSet::new(),
            findings: HashMap::new(),
            comments: HashMap::new(),
            fold_collapsed: HashSet::new(),
        }
    }

    #[test]
    fn follow_deps_direct_jump_into_a_collapsed_namespace_remaps_focus_to_the_namespace_row() {
        let g = graph_fixture_with_namespace(vec![edge("outer", "inner")]);
        let mut app = app_with_graph(g, "outer");
        app.fold_collapsed.insert(NodeId::from("ns"));

        let (app, _) = update(app, Msg::FollowDeps);

        assert_eq!(
            app.focus,
            NodeId::from("ns"),
            "focus must land on the collapsed namespace's row, not the absorbed leaf"
        );
    }

    #[test]
    fn follow_deps_direct_jump_with_no_fold_collapsed_is_unaffected() {
        let g = graph_fixture_with_namespace(vec![edge("outer", "inner")]);
        let app = app_with_graph(g, "outer");

        let (app, _) = update(app, Msg::FollowDeps);

        assert_eq!(app.focus, NodeId::from("inner"));
    }

    #[test]
    fn picker_select_into_a_collapsed_namespace_remaps_focus_to_the_namespace_row() {
        let g = graph_fixture_with_namespace(vec![edge("outer", "inner"), edge("outer", "inner2")]);
        let mut app = app_with_graph(g, "outer");
        app.fold_collapsed.insert(NodeId::from("ns"));

        let (app, _) = update(app, Msg::FollowDeps);
        let picker = app
            .picker
            .clone()
            .expect("two candidates should open a picker");
        assert_eq!(
            picker.candidates,
            vec![NodeId::from("inner"), NodeId::from("inner2")]
        );

        let (app, _) = update(app, Msg::PickerSelect);

        assert_eq!(
            app.focus,
            NodeId::from("ns"),
            "picker selection must remap into the collapsed namespace's row too"
        );
    }

    #[test]
    fn picker_select_with_no_fold_collapsed_is_unaffected() {
        let g = graph_fixture_with_namespace(vec![edge("outer", "inner"), edge("outer", "inner2")]);
        let app = app_with_graph(g, "outer");

        let (app, _) = update(app, Msg::FollowDeps);
        let (app, _) = update(app, Msg::PickerSelect);

        assert_eq!(app.focus, NodeId::from("inner"));
    }

    // -- Fix: file-less rows (review feedback) -----------------------------
    //
    // A collapsed namespace row's id names a synthetic, file-less
    // container. `Enter`/`d` on such a row used to emit `Cmd::LoadFile`/
    // `Cmd::LoadDiff` for a node with zero files -- `open_file`/`open_diff`
    // now no-op instead (see `has_files`'s doc for why this is provably
    // unreachable on the GUI).

    #[test]
    fn open_file_noop_on_a_file_less_focused_node() {
        let g = graph_fixture_with_namespace(vec![]);
        let mut app = app_with_graph(g, "outer");
        // A collapsed namespace row -- the one legitimate way `App::focus`
        // can ever be file-less (see `has_files`'s doc): `is_drawn` is
        // false for a synthetic namespace, so it must be in
        // `fold_collapsed` to satisfy `update`'s central focus-invariant
        // backstop, exactly as the real `--tui` rail view collapse flow
        // (`collapse_focused_namespace`) always arranges before setting
        // `focus` to a namespace id.
        app.fold_collapsed.insert(NodeId::from("ns"));
        app.focus = NodeId::from("ns");

        let (app, cmd) = update(app, Msg::OpenFile);

        assert_eq!(app.pane, Pane::Graph, "pane must not flip");
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn open_diff_noop_on_a_file_less_focused_node() {
        let g = graph_fixture_with_namespace(vec![]);
        let mut app = app_with_graph(g, "outer");
        // See `open_file_noop_on_a_file_less_focused_node`'s comment: a
        // collapsed namespace row is the only legitimate file-less focus.
        app.fold_collapsed.insert(NodeId::from("ns"));
        app.focus = NodeId::from("ns");

        let (app, cmd) = update(app, Msg::OpenDiff);

        assert_eq!(app.screen, Screen::Graph, "screen must not switch");
        assert_eq!(cmd, Cmd::None);
    }

    #[test]
    fn open_file_still_works_for_an_ordinary_focused_node() {
        let g = graph_fixture_with_namespace(vec![]);
        let app = app_with_graph(g, "outer");

        let (app, cmd) = update(app, Msg::OpenFile);

        assert_eq!(app.pane, Pane::File);
        assert_eq!(cmd, Cmd::LoadFile(NodeId::from("outer")));
    }

    // -- Central focus-invariant backstop (`update`'s `repair_stray_focus`) -

    /// Direct unit test of the release-build repair path, bypassing
    /// `update`'s `debug_assert!` entirely (that assert is exercised
    /// separately, in a `#[should_panic]` test below, and firing it here
    /// would abort this test before the repair logic even ran). Simulates
    /// whatever a *future* bad focus-setting arm would produce: `app.focus`
    /// pointing at an id that is neither drawn nor collapsed.
    #[test]
    fn repair_stray_focus_reseats_onto_the_first_drawn_row() {
        let g = graph_fixture_with_namespace(vec![]);
        let mut app = app_with_graph(g, "outer");
        app.focus = NodeId::from("nonexistent");

        repair_stray_focus(&mut app);

        assert!(app.is_drawn(&app.focus), "must reseat onto a drawn row");
    }

    /// A focus id that's already valid (drawn) must be left untouched --
    /// `repair_stray_focus` only kicks in on an actual invariant violation.
    #[test]
    fn repair_stray_focus_is_a_noop_for_a_valid_focus() {
        let g = graph_fixture_with_namespace(vec![]);
        let mut app = app_with_graph(g, "outer");

        repair_stray_focus(&mut app);

        assert_eq!(app.focus, NodeId::from("outer"));
    }

    /// A focus id inside `fold_collapsed` counts as valid even though it's
    /// never `is_drawn` -- mirrors `Msg::FocusSet`'s own guard.
    #[test]
    fn repair_stray_focus_is_a_noop_for_a_collapsed_namespace_focus() {
        let g = graph_fixture_with_namespace(vec![]);
        let mut app = app_with_graph(g, "outer");
        app.fold_collapsed.insert(NodeId::from("ns"));
        app.focus = NodeId::from("ns");

        repair_stray_focus(&mut app);

        assert_eq!(app.focus, NodeId::from("ns"));
    }

    /// `update`'s `debug_assert!` must fire the moment `App::focus` is
    /// stray going into a dispatch, regardless of which `Msg` triggered
    /// it -- proving the backstop catches a violation from *any* source,
    /// not just the specific `follow` bug it was written alongside.
    /// `Msg::PaneLeft` is a deliberately focus-irrelevant message: it
    /// doesn't touch `focus` at all, so this pins down that the assert
    /// checks the invariant unconditionally after every dispatch, not just
    /// after focus-setting arms.
    #[test]
    #[should_panic(expected = "is neither drawn nor a collapsed namespace")]
    fn update_debug_asserts_on_a_stray_focus_regardless_of_message() {
        let g = graph_fixture_with_namespace(vec![]);
        let mut app = app_with_graph(g, "outer");
        app.focus = NodeId::from("nonexistent");

        let _ = update(app, Msg::PaneLeft);
    }
}
