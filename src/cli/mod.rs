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
    /// Dump the project graph instead of launching the GUI.
    #[arg(long, value_enum)]
    pub dump: Option<DumpFormat>,
    /// Include per-node diff content in `--dump json` output (the
    /// AI-review payload). Requires `--dump json`; a `--dump text` run with
    /// this flag set is a friendly CLI error, not a silent no-op.
    #[arg(long, requires = "dump")]
    pub include_diffs: bool,
    /// Startup self-test: open the GUI window, then close it after a couple
    /// seconds and exit 0. Used to sanity-check that the window opens
    /// without hanging around for a human to close it manually.
    #[arg(long, hide = true)]
    pub smoke: bool,
    /// Show the full module graph, not just changes and connecting paths.
    /// By default vdiff filters to a focused view (see
    /// [`crate::graph::filter::focus_on_changes`]); this flag opts back into
    /// the unfiltered graph, for both the GUI and `--dump`.
    #[arg(long)]
    pub all: bool,
    /// Replace the built-in read-only file viewer with a real embedded
    /// `nvim --embed` instance (see [`crate::nvim`]). On by default. Falls
    /// back to the built-in viewer with a stderr warning if no `nvim`
    /// binary is found on `PATH`. Pass `--no-nvim` to opt out and use the
    /// legacy built-in viewer instead.
    #[arg(long = "no-nvim", action = clap::ArgAction::SetFalse)]
    pub nvim: bool,
    /// Ex command to run in the embedded nvim after startup (repeatable --
    /// pass `--nvim-cmd` multiple times to run several, in order). Runs
    /// after `nvim_ui_attach` and again after every automatic respawn (see
    /// [`crate::nvim::session`]'s liveness handling), before the first file
    /// opens. A failing command is logged to stderr as a warning, never
    /// fatal. Example: `--nvim-cmd ContextWindowHide` to silence a plugin
    /// that's noisy only inside vdiff's embedded instance. Ignored (with no
    /// warning) unless a usable `nvim` was found (nvim mode is on by
    /// default) and `--no-nvim` wasn't given.
    #[arg(long = "nvim-cmd")]
    pub nvim_cmd: Vec<String>,
    /// Print every captured review comment (see
    /// [`crate::review::comments`]) as markdown to stdout instead of
    /// launching the GUI or dumping the graph, then exit. Headless, like
    /// `--dump` -- reads `.git/vdiff/comments.json` directly, no graph
    /// build needed. Exits 0 with a "No comments." message if the store is
    /// empty or hasn't been created yet.
    #[arg(long)]
    pub export_comments: bool,
}

/// `--dump` output format.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpFormat {
    Text,
    Json,
}
