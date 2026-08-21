//! vdiff — visual PR review: a node graph of a branch's change set.

pub mod cli;
pub mod core;
pub mod diffing;
pub mod graph;
pub mod keymap;
pub mod nvim;
pub mod pipeline;
pub mod review;
#[cfg(feature = "tui")]
pub mod tui;
#[cfg(feature = "gui")]
pub mod ui;
