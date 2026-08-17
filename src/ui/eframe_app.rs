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
//! The embedded-nvim spike (`--nvim`, see [`crate::nvim`]) is the other
//! exception: when [`VdiffApp::nvim`] is `Some` and keyboard focus is on
//! [`Pane::File`], raw egui input bypasses `map_key` entirely in favor of
//! [`VdiffApp::handle_nvim_keys`] -- see that method's doc for why, and for
//! the two bindings (`Ctrl-w h`/`Ctrl-w l`) it still intercepts locally
//! rather than forwarding to nvim. `core::App`/`update` never learn nvim
//! mode exists: [`Cmd::LoadFile`] is translated to
//! [`crate::nvim::session::NvimCmd::OpenFile`] here in [`VdiffApp::execute`],
//! with an empty [`FileViewState`] dispatched back through the reducer just
//! to keep its `file_view.is_some()`-gated invariants (the file pane is
//! open, `Ctrl-w l` has something to switch to) satisfied without teaching
//! `core` a second file-viewing mode.

use std::time::{Duration, Instant};

use egui::{Align2, Context, Event, Key, Modifiers};

use crate::core::app::{update, App, Cmd, Msg, Pane, Screen};
use crate::core::diff_state::{DiffPaneState, FileEntry};
use crate::core::file_view::{FileViewEntry, FileViewState};
use crate::graph::layout::{layout, LayoutResult};
use crate::graph::model::{GitStatus, ModuleNode, NodeId, ProjectGraph};
use crate::keymap::{map_key, KeyContext, KeyInput, KeyOutcome, Pending};
use crate::nvim::session::NvimCmd;
use crate::pipeline::file_diff::{changed_head_ranges, load_file_diff};
use crate::pipeline::repo::GitRepo;
use crate::ui::diff_view;
use crate::ui::file_view;
use crate::ui::graph_view::{self, Transform};
use crate::ui::nvim_pane::{self, NvimPane};

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
    pending_key: Option<Pending>,
    smoke: bool,
    started_at: Instant,
    diff_loader: DiffLoader,
    /// The live embedded-nvim session, if `--nvim` was given and an `nvim`
    /// binary was found (see `main.rs`'s startup decision). Once `Some`,
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
}

/// Everything [`VdiffApp::new`] needs to set up (and later respawn) the
/// embedded-nvim spike, bundled to keep the constructor's arg count sane.
/// `pane` is `None` when `--nvim` wasn't given or no `nvim` binary was
/// found -- see `main.rs`'s startup decision -- in which case the rest of
/// the fields are unused.
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
    ) -> Self {
        Self {
            app,
            layout,
            transform: Transform::default(),
            last_focus: None,
            pending_key: None,
            smoke,
            started_at: Instant::now(),
            diff_loader,
            nvim: nvim.pane,
            nvim_ctrl_w_pending: false,
            nvim_cwd: nvim.cwd,
            nvim_init_cmds: nvim.init_cmds,
            egui_ctx: nvim.egui_ctx,
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
            Cmd::LoadFile(node) => self.load_file(node),
            Cmd::Relayout => {
                self.layout = layout(&self.app.visible_graph());
            }
        }
    }

    /// Handle [`Cmd::LoadFile`]: in nvim mode, respawn first if the session
    /// died since it was last used (see [`Self::respawn_nvim`]), then tell
    /// it to open the node's first file (line 1) via [`NvimPane::open_file`]
    /// and feed the reducer an empty [`FileViewState`] just to keep its
    /// `file_view.is_some()` invariants satisfied (see the module doc);
    /// otherwise the built-in viewer's real load, unchanged.
    fn load_file(&mut self, node: NodeId) {
        if self.nvim.is_some() {
            if self.nvim.as_ref().is_some_and(|nvim| !nvim.is_alive()) {
                self.respawn_nvim();
            }
            if let Some(nvim) = &self.nvim {
                if let Some(path) = self
                    .app
                    .graph
                    .node(&node)
                    .and_then(|module| module.files.first())
                    .map(|file_ref| file_ref.path.clone())
                {
                    nvim.open_file(path, Some(1));
                }
            }
            self.dispatch(Msg::FileLoaded(FileViewState::new(node, Vec::new())));
            return;
        }
        match self.diff_loader.load_file_view(&self.app.graph, &node) {
            Ok(state) => self.dispatch(Msg::FileLoaded(state)),
            Err(message) => self.dispatch(Msg::FileLoadFailed(message)),
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
                for cmd in &self.nvim_init_cmds {
                    pane.send(NvimCmd::Ex(cmd.clone()));
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
    /// see that method's doc.
    fn handle_keys(&mut self, ctx: &Context) {
        if self.nvim.is_some() && self.app.pane == Pane::File {
            self.handle_nvim_keys(ctx);
            return;
        }
        let presses: Vec<(Key, Modifiers)> = ctx.input(|i| {
            i.events
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
        });

        for (key, modifiers) in presses {
            let Some(input) = egui_key_to_input(key, modifiers) else {
                continue;
            };
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

    /// The nvim-mode input path: every raw egui event this frame either
    /// completes/starts the local `Ctrl-w` chord (intercepted, never
    /// forwarded -- `h` dispatches [`Msg::PaneLeft`], `l` dispatches
    /// [`Msg::PaneRight`] exactly like [`map_key`]'s `Ctrl-w` binding does
    /// for the built-in viewer, anything else clears the chord with no
    /// effect) or gets translated by
    /// [`crate::ui::nvim_pane::translate_event_for_nvim`] and sent to the
    /// session as [`NvimCmd::Input`]. `map_key`/the reducer are bypassed
    /// entirely for everything else -- nvim owns its own modal keymap, and
    /// `core::App`'s `File*` messages (scroll, half-page, change/file jump)
    /// have nothing to act on here since there's no [`FileViewState`]
    /// content backing this pane.
    fn handle_nvim_keys(&mut self, ctx: &Context) {
        let events = ctx.input(|i| i.events.clone());
        for event in &events {
            if self.nvim_ctrl_w_pending {
                self.nvim_ctrl_w_pending = false;
                if let Event::Key {
                    key, pressed: true, ..
                } = event
                {
                    match key {
                        Key::H => self.dispatch(Msg::PaneLeft),
                        Key::L => self.dispatch(Msg::PaneRight),
                        _ => {}
                    }
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
                    self.nvim_ctrl_w_pending = true;
                    continue;
                }
            }
            if let Some(text) = nvim_pane::translate_event_for_nvim(event) {
                if let Some(nvim) = &self.nvim {
                    nvim.send(NvimCmd::Input(text));
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

    /// [`Screen::Graph`] with [`App::file_view`] open: the right-hand pane
    /// on a resizable [`egui::SidePanel`], then the graph on whatever
    /// [`egui::CentralPanel`] space remains. In nvim mode the pane renders
    /// [`nvim_pane::show`] instead of the built-in [`file_view::show`] --
    /// see the module doc for why `App::file_view` being (emptily) `Some`
    /// still gates this the same way for both. Records the row count
    /// [`file_view::show`] fit into that space as [`App::viewport_rows`],
    /// read one frame later by [`Msg::FileHalfPage`] -- see that field's
    /// doc for why this can't be computed any earlier; not meaningful in
    /// nvim mode (`Ctrl-d`/`Ctrl-u` are forwarded raw, see
    /// [`Self::handle_nvim_keys`]), so left untouched there.
    fn show_two_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.app.file_view.is_some() {
            let focused = self.app.pane == Pane::File;
            if let Some(nvim) = self.nvim.as_mut() {
                egui::Panel::right("file_pane")
                    .resizable(true)
                    .default_size(ui.available_width() * 0.45)
                    .show(ui, |ui| nvim_pane::show(ui, nvim, focused));
            } else if let Some(file_view) = self.app.file_view.as_ref() {
                let response = egui::Panel::right("file_pane")
                    .resizable(true)
                    .default_size(ui.available_width() * 0.45)
                    .show(ui, |ui| file_view::show(ui, file_view, focused));
                self.app.viewport_rows = response.inner;
            }
        }
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
        self.handle_keys(ctx);
        self.handle_zoom_keys(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.app.screen {
            Screen::Graph => self.show_two_panel(ui),
            Screen::Diff => {
                egui::CentralPanel::default().show(ui, |ui| {
                    self.show_diff(ui);
                });
            }
        }
    }
}

/// Translate an egui key press (with its modifiers) to vdiff's
/// toolkit-independent [`KeyInput`]. Pure and unit-tested: with Ctrl held,
/// only `w`/`d`/`u` map to anything ([`KeyInput::Ctrl`]); otherwise the
/// keys [`crate::keymap::map_key`] cares about (h/j/k/l/g/G/d/r/t/s/c/f/[/],
/// Enter, Esc) map to anything, everything else is `None`. `Key::G` maps to
/// `Char('G')` when Shift is held (uppercase, distinct from the `gg`/`gd`/
/// `gr` prefix `Char('g')`) and `Char('g')` otherwise.
pub fn egui_key_to_input(key: Key, modifiers: Modifiers) -> Option<KeyInput> {
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
        Key::F => Some(KeyInput::Char('f')),
        Key::OpenBracket => Some(KeyInput::Char('[')),
        Key::CloseBracket => Some(KeyInput::Char(']')),
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
