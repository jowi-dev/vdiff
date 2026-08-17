//! Pure Elm-style state and reducer for vdiff's application core.
//!
//! [`App`] holds all state, [`Msg`] is every event [`update`] can react to,
//! and `update` is the single place state transitions happen. No I/O occurs
//! here: [`Cmd`] names the I/O the caller (the eframe glue, in a later
//! chunk) should perform next; `DiffLoaded`/`LoadFailed` feed its result back
//! in without `update` ever needing to touch git/egui itself.

pub use crate::core::diff_state::DiffPaneState;
use crate::core::focus::{dep_targets, dependent_sources, move_focus, Direction};
use crate::graph::model::{NodeId, ProjectGraph};

/// Which screen is currently shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The node graph.
    Graph,
    /// The diff pane for the node focused when it was opened.
    Diff,
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
}

impl App {
    /// Whether `id` is a drawn (real, navigable) node -- present in
    /// `self.layers` -- as opposed to a synthetic namespace node or an
    /// unknown id. [`Msg::FocusSet`] rejects any target that isn't.
    fn is_drawn(&self, id: &NodeId) -> bool {
        self.layers.iter().any(|layer| layer.contains(id))
    }
}

/// Every event [`update`] can react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// h/j/k/l tree-walk step. Only acted on on [`Screen::Graph`] with no
    /// picker open.
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
    /// later).
    LoadFailed(String),
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
}

/// Advance `app` in response to `msg`, returning the new state and any
/// command the caller should execute. Pure: performs no I/O.
pub fn update(mut app: App, msg: Msg) -> (App, Cmd) {
    match msg {
        Msg::FocusMove(dir) => {
            if on_graph_with_no_picker(&app) {
                app.focus = move_focus(&app.layers, &app.focus, dir);
            }
            (app, Cmd::None)
        }
        Msg::FocusSet(id) => {
            if on_graph_with_no_picker(&app) && app.is_drawn(&id) {
                app.focus = id;
            }
            (app, Cmd::None)
        }
        Msg::FollowDeps => follow(app, dep_targets),
        Msg::FollowDependents => follow(app, dependent_sources),
        Msg::PickerMove(delta) => {
            picker_move(&mut app, delta);
            (app, Cmd::None)
        }
        Msg::PickerSelect => {
            picker_select(&mut app);
            (app, Cmd::None)
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
    }
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

/// Whether `app` is in the state [`Msg::FocusMove`]/[`Msg::FocusSet`]/
/// [`Msg::FollowDeps`]/[`Msg::FollowDependents`]/[`Msg::OpenDiff`] require:
/// on [`Screen::Graph`], with no picker overlay open.
fn on_graph_with_no_picker(app: &App) -> bool {
    app.screen == Screen::Graph && app.picker.is_none()
}

/// Shared handler for [`Msg::FollowDeps`]/[`Msg::FollowDependents`]: look up
/// `candidates` via `edges_fn`, then no-op/jump/open-picker per how many
/// there are.
fn follow(mut app: App, edges_fn: impl Fn(&ProjectGraph, &NodeId) -> Vec<NodeId>) -> (App, Cmd) {
    if !on_graph_with_no_picker(&app) {
        return (app, Cmd::None);
    }
    let candidates = edges_fn(&app.graph, &app.focus);
    match candidates.len() {
        0 => {}
        1 => app.focus = candidates.into_iter().next().expect("checked len == 1"),
        _ => {
            app.picker = Some(EdgePicker {
                candidates,
                selected: 0,
            })
        }
    }
    (app, Cmd::None)
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
}
