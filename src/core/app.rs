//! Pure Elm-style state and reducer for vdiff's application core.
//!
//! [`App`] holds all state, [`Msg`] is every event [`update`] can react to,
//! and `update` is the single place state transitions happen. No I/O occurs
//! here: [`Cmd`] names the I/O the caller (the eframe glue, in a later
//! chunk) should perform next; `DiffLoaded`/`LoadFailed` feed its result back
//! in without `update` ever needing to touch git/egui itself.

pub use crate::core::diff_state::DiffPaneState;
use crate::core::file_view::FileViewState;
use crate::core::focus::{dep_targets, dependent_sources, move_focus, Direction};
use crate::graph::layers::assign_layers;
use crate::graph::model::{NodeId, ProjectGraph};
use crate::graph::test_modules::hide_test_modules;

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
}

impl App {
    /// Whether `id` is a drawn (real, navigable) node -- present in
    /// `self.layers` -- as opposed to a synthetic namespace node or an
    /// unknown id. [`Msg::FocusSet`] rejects any target that isn't.
    fn is_drawn(&self, id: &NodeId) -> bool {
        self.layers.iter().any(|layer| layer.contains(id))
    }

    /// The graph actually drawn: `self.graph` as-is if [`Self::show_tests`],
    /// or with every test module pruned out (see
    /// [`crate::graph::test_modules::hide_test_modules`]) otherwise. `self.graph`
    /// itself never changes -- it's always the full, focus-filtered graph --
    /// so this is what [`Msg::ToggleTests`] recomputes `layers` from, and
    /// what the caller should re-run [`crate::graph::layout::layout`] over
    /// after a [`Cmd::Relayout`].
    pub fn visible_graph(&self) -> ProjectGraph {
        if self.show_tests {
            self.graph.clone()
        } else {
            hide_test_modules(&self.graph).0
        }
    }
}

/// Every event [`update`] can react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// h/j/k/l layer navigation step (see [`crate::core::focus`]). Only
    /// acted on on [`Screen::Graph`] with no picker open.
    FocusMove(Direction),
    /// Jump focus directly to a node. Only acted on on [`Screen::Graph`]
    /// with no picker open, and only if the node exists in the graph.
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
}

/// Advance `app` in response to `msg`, returning the new state and any
/// command the caller should execute. Pure: performs no I/O.
pub fn update(mut app: App, msg: Msg) -> (App, Cmd) {
    match msg {
        Msg::FocusMove(dir) => {
            if !on_graph_with_no_picker_and_graph_pane(&app) {
                return (app, Cmd::None);
            }
            let old_focus = app.focus.clone();
            app.focus = move_focus(&app.layers, &app.focus, dir);
            let cmd = reload_file_on_focus_change(&app, &old_focus);
            (app, cmd)
        }
        Msg::FocusSet(id) => {
            if !on_graph_with_no_picker_and_graph_pane(&app) || !app.is_drawn(&id) {
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
    }
}

/// Handle [`Msg::ToggleTests`]: flip `show_tests`, recompute `layers` from
/// [`App::visible_graph`], and re-seat focus (see [`reseat_focus`]) if it's
/// no longer drawn.
fn toggle_tests(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    app.show_tests = !app.show_tests;
    let old_layers = std::mem::take(&mut app.layers);
    app.layers = assign_layers(&app.visible_graph());
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
fn follow(mut app: App, edges_fn: impl Fn(&ProjectGraph, &NodeId) -> Vec<NodeId>) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    let old_focus = app.focus.clone();
    let candidates = edges_fn(&app.graph, &app.focus);
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

/// Handle [`Msg::OpenDiff`]: only on [`Screen::Graph`] with no picker open,
/// switch to [`Screen::Diff`] and emit [`Cmd::LoadDiff`] for the focused
/// node.
fn open_diff(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker(&app) {
        return (app, Cmd::None);
    }
    let focus = app.focus.clone();
    app.screen = Screen::Diff;
    (app, Cmd::LoadDiff(focus))
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
/// synchronously within the same dispatch, so there's no visible gap.
fn open_file(mut app: App) -> (App, Cmd) {
    if !on_graph_with_no_picker_and_graph_pane(&app) {
        return (app, Cmd::None);
    }
    let focus = app.focus.clone();
    app.pane = Pane::File;
    (app, Cmd::LoadFile(focus))
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
        let layers = crate::graph::layers::assign_layers(&graph);
        App {
            graph,
            layers,
            focus: NodeId::from(focus),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
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
        let app = app_at("root");
        let (app, _) = update(app, Msg::FollowDependents);
        assert_eq!(app.focus, NodeId::from("root"));
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

    #[test]
    fn toggle_tests_reveals_hidden_test_node_and_recomputes_layers() {
        let g = graph_fixture_with_test_node();
        let visible = crate::graph::test_modules::hide_test_modules(&g).0;
        let app = App {
            graph: g,
            layers: crate::graph::layers::assign_layers(&visible),
            focus: NodeId::from("leaf_a"),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
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
        let app = App {
            layers: crate::graph::layers::assign_layers(&g),
            graph: g,
            focus: NodeId::from("test_x"),
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: true,
            file_view: None,
            pane: Pane::Graph,
            viewport_rows: 20,
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
}
