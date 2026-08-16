//! Command-line entry point: parse args, build a real [`Git2Repo`], and
//! dump the resulting [`ProjectGraph`] as text or JSON. There's no GUI yet
//! -- `--dump` is required.

pub mod dump;

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

pub use dump::render;

/// `vdiff [--repo <path>] [--base <ref>] --dump <text|json>`.
#[derive(Parser, Debug)]
#[command(
    name = "vdiff",
    about = "Visual PR review: a node graph of a branch's change set"
)]
pub struct Cli {
    /// Path inside the repository to review (defaults to the current
    /// directory).
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Diff base ref override (defaults to the detected default branch).
    #[arg(long)]
    pub base: Option<String>,
    /// Dump the project graph instead of launching the GUI (not yet
    /// built, so this is required for now).
    #[arg(long, value_enum)]
    pub dump: Option<DumpFormat>,
}

/// `--dump` output format.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpFormat {
    Text,
    Json,
}
