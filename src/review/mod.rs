//! Local review comments (roadmap issue #7's first slice): a comment
//! captured while reviewing -- either anchored to a line range in a file or
//! to a whole graph node (`c` on the focused node in the graph pane, see
//! [`crate::core::app::Msg::CommentNode`]) -- lives at
//! `<git_dir>/vdiff/comments.json` (see
//! [`crate::pipeline::repo::GitRepo::git_dir`] for why this is the repo's
//! actual git directory rather than `<worktree>/.git` joined by hand), and
//! exports as markdown (`vdiff --export-comments`).
//!
//! This crate no longer captures comments itself: `:VdiffComment`, the
//! compose UI, and writing `comments.json` are owned by the standalone
//! `vdiff.nvim` plugin (github.com/jowi-dev/vdiff.nvim), which loads inside
//! the embedded nvim pane automatically since it runs the user's own nvim
//! config -- `c` on a focused graph node just delegates to it (see
//! [`crate::ui::nvim_pane::NvimPane::delegate_comment_node`]). This module
//! is the reading half of that shared contract: [`comments`] is the pure
//! data model (struct, sort/id/render helpers, fully unit-tested) and
//! [`store`] is the thin glue-side IO wrapper around it, both used by
//! `vdiff --export-comments`. See `docs/comments-schema.md` for the wire
//! contract itself.
//!
//! [`review_state`] is the sibling data model backing review-completion
//! tracking (issue #4): which nodes have been marked reviewed, keyed by
//! branch and guarded by a per-node file fingerprint so a stale mark never
//! survives the files it was made against changing underneath it. Lives at
//! `<git_dir>/vdiff/review-state.json`, alongside `comments.json`, via the
//! same [`store`] module. Unlike comments, the toggle/compose/paint side of
//! this feature is still owned by this crate (see
//! [`crate::core::app::Msg::ToggleReviewed`]) -- there's no nvim-plugin
//! delegation involved.
//!
//! [`findings`] closes the loop the opposite direction from `--dump json
//! --include-diffs` (the AI-review *input* payload): `vdiff --findings
//! <path>` reads a review agent's *output* -- a JSON list of [`Finding`]s
//! keyed by node id or file path -- and renders it on the graph (a severity
//! badge), the focus overlay (finding summaries for the focused node), and
//! the built-in file pane (per-line markers). See
//! `docs/findings-schema.md` for the wire contract. Loaded once at startup
//! in `main.rs`; `core::App` only ever holds the already-mapped
//! `HashMap<NodeId, Vec<Finding>>`, never the raw file or path-matching
//! logic.
//!
//! [`Finding`]: findings::Finding

pub mod comments;
pub mod findings;
pub mod review_state;
pub mod store;
