//! [`eframe::App`] glue: owns [`core::App`] plus view-only state (the pan/
//! zoom [`Transform`] and the pending `g`-chord char), translates egui key
//! events into [`crate::keymap::KeyInput`], threads them through
//! [`crate::keymap::map_key`] and [`crate::core::app::update`], and executes
//! the resulting [`Cmd`]. All I/O and toolkit-specific state lives here --
//! `core::App`/`update` stay pure.

use std::time::{Duration, Instant};

use egui::{Align2, Context, Key};

use crate::core::app::{update, App, Cmd, DiffPaneState, Msg, Screen};
use crate::graph::layout::LayoutResult;
use crate::keymap::{map_key, KeyContext, KeyInput, KeyOutcome};
use crate::ui::graph_view::{self, Transform};

/// How long `--smoke` keeps the window open before closing it.
const SMOKE_DURATION: Duration = Duration::from_secs(2);

/// Owns [`core::App`] and drives it from egui input/paint each frame.
pub struct VdiffApp {
    app: App,
    layout: LayoutResult,
    transform: Transform,
    pending_key: Option<char>,
    smoke: bool,
    started_at: Instant,
}

impl VdiffApp {
    /// Build a fresh GUI app wrapping an already-constructed [`App`] and its
    /// [`LayoutResult`]. `smoke` enables the self-closing startup self-test
    /// (see the module-level `--smoke` flag in `main.rs`).
    pub fn new(app: App, layout: LayoutResult, smoke: bool) -> Self {
        Self {
            app,
            layout,
            transform: Transform::default(),
            pending_key: None,
            smoke,
            started_at: Instant::now(),
        }
    }

    /// Dispatch `msg` through the pure reducer and execute the resulting
    /// [`Cmd`].
    fn dispatch(&mut self, msg: Msg) {
        let (app, cmd) = update(self.app.clone(), msg);
        self.app = app;
        self.execute(cmd);
    }

    /// Execute a [`Cmd`]. `LoadDiff` has no real diffing pipeline yet
    /// (chunk E) -- immediately report an empty [`DiffPaneState`] back
    /// through the reducer, matching the plan's chunk D placeholder.
    fn execute(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::None => {}
            Cmd::LoadDiff(_node) => self.dispatch(Msg::DiffLoaded(DiffPaneState::default())),
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
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.app.screen {
            Screen::Graph => {
                let ctx = ui.ctx().clone();
                egui::CentralPanel::default().show(ui, |ui| {
                    graph_view::show(ui, &self.app, &self.layout, &mut self.transform);
                });
                self.show_picker(&ctx);
            }
            Screen::Diff => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("diff pane — chunk E");
                });
            }
        }
    }
}

/// Translate an egui key press to vdiff's toolkit-independent [`KeyInput`].
/// Pure and unit-tested: only the keys [`crate::keymap::map_key`] cares
/// about (h/j/k/l/g/d/r, Enter, Esc) map to anything; everything else is
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
        Key::Enter => Some(KeyInput::Enter),
        Key::Escape => Some(KeyInput::Esc),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
