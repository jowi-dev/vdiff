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

use std::time::{Duration, Instant};

use egui::{Align2, Context, Key};

use crate::core::app::{update, App, Cmd, Msg, Screen};
use crate::core::diff_state::{DiffPaneState, FileEntry};
use crate::graph::layout::{layout, LayoutResult};
use crate::graph::model::{NodeId, ProjectGraph};
use crate::keymap::{map_key, KeyContext, KeyInput, KeyOutcome};
use crate::pipeline::file_diff::load_file_diff;
use crate::pipeline::repo::GitRepo;
use crate::ui::diff_view;
use crate::ui::graph_view::{self, Transform};

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
}

/// Owns [`core::App`] and drives it from egui input/paint each frame.
pub struct VdiffApp {
    app: App,
    layout: LayoutResult,
    transform: Transform,
    /// The focus [`graph_view::show`]'s auto-pan last ran for -- lets it
    /// fire only when focus actually changes rather than every repaint.
    last_focus: Option<NodeId>,
    pending_key: Option<char>,
    smoke: bool,
    started_at: Instant,
    diff_loader: DiffLoader,
}

impl VdiffApp {
    /// Build a fresh GUI app wrapping an already-constructed [`App`] and its
    /// [`LayoutResult`]. `smoke` enables the self-closing startup self-test
    /// (see the module-level `--smoke` flag in `main.rs`). `diff_loader`
    /// backs [`Cmd::LoadDiff`].
    pub fn new(app: App, layout: LayoutResult, smoke: bool, diff_loader: DiffLoader) -> Self {
        Self {
            app,
            layout,
            transform: Transform::default(),
            last_focus: None,
            pending_key: None,
            smoke,
            started_at: Instant::now(),
            diff_loader,
        }
    }

    /// Dispatch `msg` through the pure reducer and execute the resulting
    /// [`Cmd`].
    fn dispatch(&mut self, msg: Msg) {
        let (app, cmd) = update(self.app.clone(), msg);
        self.app = app;
        self.execute(cmd);
    }

    /// Execute a [`Cmd`]: `LoadDiff` reads file content via `diff_loader`
    /// and reports the result back through the reducer as `DiffLoaded`/
    /// `LoadFailed`; `Relayout` rebuilds `self.layout` from
    /// [`App::visible_graph`] now that `self.app.layers` changed shape.
    fn execute(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::None => {}
            Cmd::LoadDiff(node) => match self.diff_loader.load(&self.app.graph, &node) {
                Ok(state) => self.dispatch(Msg::DiffLoaded(state)),
                Err(message) => self.dispatch(Msg::LoadFailed(message)),
            },
            Cmd::Relayout => {
                self.layout = layout(&self.app.visible_graph());
            }
        }
    }

    /// Read this frame's key-press events, translate and route each through
    /// [`map_key`], updating the pending chord char and dispatching any
    /// resulting [`Msg`].
    fn handle_keys(&mut self, ctx: &Context) {
        let presses: Vec<Key> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
                .collect()
        });

        for key in presses {
            let Some(input) = egui_key_to_input(key) else {
                continue;
            };
            let ctx = KeyContext {
                screen: self.app.screen,
                picker_open: self.app.picker.is_some(),
                pending: self.pending_key,
            };
            let outcome = map_key(input, ctx);
            self.pending_key = None;
            match outcome {
                KeyOutcome::Msg(msg) => self.dispatch(msg),
                KeyOutcome::Pending(c) => self.pending_key = Some(c),
                KeyOutcome::None => {}
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

        self.handle_keys(ctx);
        self.handle_zoom_keys(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.app.screen {
            Screen::Graph => {
                let ctx = ui.ctx().clone();
                egui::CentralPanel::default().show(ui, |ui| {
                    graph_view::show(
                        ui,
                        &self.app,
                        &self.layout,
                        &mut self.transform,
                        &mut self.last_focus,
                    );
                });
                self.show_picker(&ctx);
            }
            Screen::Diff => {
                egui::CentralPanel::default().show(ui, |ui| {
                    self.show_diff(ui);
                });
            }
        }
    }
}

/// Translate an egui key press to vdiff's toolkit-independent [`KeyInput`].
/// Pure and unit-tested: only the keys [`crate::keymap::map_key`] cares
/// about (h/j/k/l/g/d/r/t, Enter, Esc) map to anything; everything else is
/// `None`.
pub fn egui_key_to_input(key: Key) -> Option<KeyInput> {
    match key {
        Key::H => Some(KeyInput::Char('h')),
        Key::J => Some(KeyInput::Char('j')),
        Key::K => Some(KeyInput::Char('k')),
        Key::L => Some(KeyInput::Char('l')),
        Key::G => Some(KeyInput::Char('g')),
        Key::D => Some(KeyInput::Char('d')),
        Key::R => Some(KeyInput::Char('r')),
        Key::T => Some(KeyInput::Char('t')),
        Key::Enter => Some(KeyInput::Enter),
        Key::Escape => Some(KeyInput::Esc),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::repo::FakeRepo;

    #[test]
    fn translates_mapped_keys() {
        let cases = [
            (Key::H, KeyInput::Char('h')),
            (Key::J, KeyInput::Char('j')),
            (Key::K, KeyInput::Char('k')),
            (Key::L, KeyInput::Char('l')),
            (Key::G, KeyInput::Char('g')),
            (Key::D, KeyInput::Char('d')),
            (Key::R, KeyInput::Char('r')),
            (Key::T, KeyInput::Char('t')),
            (Key::Enter, KeyInput::Enter),
            (Key::Escape, KeyInput::Esc),
        ];
        for (key, expected) in cases {
            assert_eq!(egui_key_to_input(key), Some(expected), "key={key:?}");
        }
    }

    #[test]
    fn unmapped_keys_translate_to_none() {
        for key in [Key::A, Key::Z, Key::Num1, Key::Space, Key::Tab] {
            assert_eq!(egui_key_to_input(key), None, "key={key:?}");
        }
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
}
