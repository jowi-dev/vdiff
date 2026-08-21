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
    /// When combined with `--pr`, this wins over the PR's own base branch
    /// (see [`crate::pipeline::pr`]'s module doc).
    #[arg(long)]
    pub base: Option<String>,
    /// Review a GitHub PR by number: resolves it via `gh pr view` (`gh`
    /// must be installed and authenticated), fetches its head into a
    /// temporary git worktree (never touching this checkout), and opens
    /// vdiff there with `--base` defaulted to the PR's base branch. The
    /// temporary worktree is removed on exit if it has no local
    /// modifications; see [`crate::pipeline::pr`].
    #[arg(long)]
    pub pr: Option<u64>,
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
    /// Load AI review findings (see [`crate::review::findings`]) from a
    /// JSON file and render them on the graph -- a severity badge per
    /// flagged node, summaries in the focus overlay, and per-line markers
    /// in the built-in file pane. See `docs/findings-schema.md` for the
    /// wire contract. Conflicts with `--dump`: findings are a GUI-only
    /// rendering feature, and `--dump`'s headless graph/JSON output has
    /// nothing to render them onto.
    #[arg(long, conflicts_with = "dump")]
    pub findings: Option<PathBuf>,
    /// Batch-publish every captured review comment (see
    /// [`crate::review::comments`]) to GitHub PR `<n>` as a single review:
    /// diff-anchored comments become GitHub line comments, everything else
    /// lands in the review body under a "Comments outside the diff"
    /// section (see [`crate::review::publish`]). Headless, like
    /// `--export-comments` -- no GUI, and unlike `--pr`, no temporary
    /// worktree checkout: comments were captured against the *current*
    /// worktree, which is assumed to already be PR `<n>`'s head (give or
    /// take local edits). Already-published comments (tracked in
    /// `<git_dir>/vdiff/published-comments.json`, scoped per PR number)
    /// are skipped unless `--republish` is given. Conflicts with `--dump`,
    /// `--pr`, and `--export-comments`.
    #[arg(
        long,
        conflicts_with_all = ["dump", "pr", "export_comments"]
    )]
    pub publish_comments: Option<u64>,
    /// With `--publish-comments`, print the plan -- every line comment
    /// that would be posted and the review body -- without touching `gh`
    /// at all, then exit 0. Meaningless without `--publish-comments`,
    /// which clap enforces.
    #[arg(long, requires = "publish_comments")]
    pub dry_run: bool,
    /// With `--publish-comments`, ignore the published-comments sidecar
    /// and post every matching comment again, even ones already recorded
    /// as published to this PR.
    #[arg(long, requires = "publish_comments")]
    pub republish: bool,
    /// Launch the ratatui/crossterm terminal frontend (issue #16) instead
    /// of the default egui/eframe GUI: a focused-neighborhood view of one
    /// module at a time (the module plus its direct dependencies/
    /// dependents), not a ported graph canvas -- see `crate::tui`'s module
    /// doc. Conflicts with `--dump`/`--export-comments`/
    /// `--publish-comments`, same as the GUI path this replaces (all three
    /// are headless and never launch either frontend). Works combined with
    /// `--pr`/`--findings` exactly like the GUI does. Errors cleanly,
    /// naming the missing feature, on a build without the `tui` feature
    /// (see `src/main.rs`'s `launch_tui`).
    #[arg(long, conflicts_with_all = ["dump", "export_comments", "publish_comments"])]
    pub tui: bool,
}

/// `--dump` output format.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpFormat {
    Text,
    Json,
}
