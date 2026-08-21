//! Pure application core: focus navigation and the Elm-style App/Msg/Cmd
//! reducer over a [`crate::graph::model::ProjectGraph`]. No dependencies on
//! egui/git2/syn/tree-sitter -- only `crate::graph` and std.

pub mod app;
pub mod diff_state;
pub mod file_view;
pub mod focus;
pub mod rail_view;
pub mod review;
