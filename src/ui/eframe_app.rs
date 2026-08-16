//! [`eframe::App`] glue: owns [`core::App`] plus view-only state (the pan/
//! zoom [`Transform`]) and paints the current screen each frame. All I/O
//! and toolkit-specific state lives here -- `core::App` stays pure.

use std::time::{Duration, Instant};

use egui::Context;

use crate::core::app::App;
use crate::graph::layout::LayoutResult;
use crate::ui::graph_view::{self, Transform};

/// How long `--smoke` keeps the window open before closing it.
const SMOKE_DURATION: Duration = Duration::from_secs(2);

/// Owns [`core::App`] and drives it from egui input/paint each frame.
pub struct VdiffApp {
    app: App,
    layout: LayoutResult,
    transform: Transform,
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
            smoke,
            started_at: Instant::now(),
        }
    }
}

impl eframe::App for VdiffApp {
    /// Non-painting logic, called once before [`Self::ui`] each frame: the
    /// `--smoke` self-close timer.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if self.smoke {
            if self.started_at.elapsed() > SMOKE_DURATION {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            graph_view::show(ui, &self.app, &self.layout, &mut self.transform);
        });
    }
}
