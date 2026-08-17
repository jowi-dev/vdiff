//! GUI glue: eframe/egui rendering on top of the pure `core`/`graph` model.
//! Everything here is I/O- and toolkit-coupled by design; the pure state
//! lives in [`crate::core`].

pub mod diff_view;
pub mod eframe_app;
pub mod file_view;
pub mod graph_view;
pub mod theme;
