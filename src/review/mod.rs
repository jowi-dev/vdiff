//! Local review comments (roadmap issue #7's first slice): capture a
//! comment while reviewing -- either anchored to a line range in a file
//! (`:VdiffComment` inside the embedded nvim pane, see
//! [`crate::nvim::session`]) or to a whole graph node (`c` on the focused
//! node in the graph pane, see [`crate::core::app::Msg::CommentNode`]) --
//! store it at `<repo>/.git/vdiff/comments.json`, and export it as
//! markdown (`vdiff --export-comments`).
//!
//! [`comments`] is the pure data model (struct, sort/id/render helpers,
//! fully unit-tested); [`store`] is the thin glue-side IO wrapper around
//! it. Out of scope for this MVP (tracked as follow-ups): posting to
//! GitHub PRs, a multi-line compose buffer, editing/deleting comments from
//! the UI, and blob-identity-keyed persistence (issue #4's territory).

pub mod comments;
pub mod store;
