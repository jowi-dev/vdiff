//! Shared error type for everything under `pipeline/`: git access, source
//! extraction, and resolution. One enum per tskmstr's thiserror convention
//! rather than a per-module type, since callers (`pipeline::build_graph`)
//! need to propagate errors from all of these uniformly.

use std::path::PathBuf;

use thiserror::Error;

/// Everything that can go wrong building a [`crate::graph::model::ProjectGraph`]
/// from a repository.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// A `git2` operation failed.
    #[error("git operation failed: {0}")]
    Git(#[from] git2::Error),
    /// Reading a file from disk failed.
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A file's content wasn't valid UTF-8, so it can't be parsed as source.
    #[error("{path} is not valid UTF-8")]
    NotUtf8 { path: PathBuf },
    /// No base ref could be resolved: no override, no `origin/HEAD`, no
    /// `main`, no `master`.
    #[error("no base ref found (tried override, origin/HEAD, main, master)")]
    NoBaseRef,
    /// `path` (or any of its ancestors) isn't a git repository --
    /// `git2::Repository::discover` found nothing to open. Reported as its
    /// own friendly variant rather than the raw `git2::Error` from
    /// `discover`, whose `Display` impl tacks on a `class=...; code=...`
    /// suffix that reads as an internal detail, not a clear user-facing
    /// message.
    #[error("not a git repository: {path}")]
    NotAGitRepo { path: PathBuf },
    /// `--base <ref>` (or the default-branch detection's own tried
    /// candidates) didn't resolve to a real ref/commit via `revparse_single`.
    #[error("base ref '{base}' not found")]
    BaseRefNotFound { base: String },
}

/// Convenience alias, used throughout `pipeline/`.
pub type Result<T> = std::result::Result<T, PipelineError>;
