//! The impure half of `vdiff --inbox` (issue #36): shelling out to `gh` to
//! learn the viewer's login and run the one GraphQL query the inbox is
//! computed from. The classification itself is pure and lives in
//! [`crate::review::inbox`]; this module pairs with it the same way
//! [`super::publish`] pairs with [`crate::review::publish`].
//!
//! `gh` stays the only credential holder, the same design constraint
//! [`super::pr`]'s module doc explains for `--pr`. Nothing is written to
//! disk: every call recomputes the inbox from GitHub.

use std::path::Path;
use std::process::Command;

use thiserror::Error;

use crate::pipeline::publish::repo_name_with_owner;
use crate::review::inbox::{parse_inbox_response, InboxError, InboxReport};

/// The inbox GraphQL query. Both searches share one fragment; `$login`
/// filters `reviews` to the viewer's own submitted reviews (pending ones
/// are excluded, since they record no reviewed state yet). Page sizes stay
/// under GitHub's 500k-node query budget: 30 PRs per search, the last 100
/// commits, and the first 50 threads of 50 comments each.
const INBOX_QUERY: &str = r#"
query($requested: String!, $reviewed: String!, $login: String!) {
  requested: search(query: $requested, type: ISSUE, first: 30) { nodes { ...pr } }
  reviewed: search(query: $reviewed, type: ISSUE, first: 30) { nodes { ...pr } }
}
fragment pr on PullRequest {
  number title url updatedAt
  author { login }
  repository { nameWithOwner }
  headRefOid
  reviews(author: $login, last: 1, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {
    nodes { commit { oid } }
  }
  commits(last: 100) { nodes { commit { oid } } }
  reviewThreads(first: 50) {
    nodes { isResolved comments(first: 50) { nodes { author { login } } } }
  }
}
"#;

/// Everything that can go wrong fetching the inbox.
#[derive(Debug, Error)]
pub enum InboxFetchError {
    /// `gh` isn't on `PATH`.
    #[error(
        "`gh` (GitHub CLI) not found on PATH -- install it from https://cli.github.com to use --inbox"
    )]
    GhNotFound,
    /// A `gh` invocation exited non-zero; `gh`'s own stderr already reads
    /// as a one-line, user-facing message.
    #[error("gh failed: {0}")]
    GhFailed(String),
    /// The current repo's `owner/name` couldn't be resolved.
    #[error("couldn't resolve the current GitHub repo (pass --all-repos to skip): {0}")]
    RepoUnknown(String),
    #[error(transparent)]
    Parse(#[from] InboxError),
}

/// Fetch the inbox. With `all_repos` false it is limited to the GitHub
/// repo checked out at `repo_path` (resolved via `gh repo view`); with it
/// true, every repo the viewer can see. Runs `gh` twice or three times:
/// once for the viewer's login, once for the repo name when scoped, and
/// once for the query.
pub fn fetch_inbox(repo_path: &Path, all_repos: bool) -> Result<InboxReport, InboxFetchError> {
    let login = run_gh(repo_path, &["api", "user", "--jq", ".login"])?;
    let scope = if all_repos {
        None
    } else {
        Some(
            repo_name_with_owner(repo_path)
                .map_err(|err| InboxFetchError::RepoUnknown(err.to_string()))?,
        )
    };
    let (requested, reviewed) = search_queries(scope.as_deref());
    let output = run_gh(
        repo_path,
        &[
            "api",
            "graphql",
            "-f",
            &format!("query={INBOX_QUERY}"),
            "-f",
            &format!("requested={requested}"),
            "-f",
            &format!("reviewed={reviewed}"),
            "-f",
            &format!("login={login}"),
        ],
    )?;
    let entries = parse_inbox_response(&output, &login)?;
    Ok(InboxReport::new(scope, entries))
}

/// The two GitHub search strings the inbox query runs: PRs awaiting the
/// viewer's review, and PRs the viewer has reviewed (excluding their own).
/// `scope` is `owner/name` to limit both to one repo, or `None` for every
/// repo the viewer can see.
pub fn search_queries(scope: Option<&str>) -> (String, String) {
    let base = "is:pr is:open archived:false";
    let repo = scope.map(|s| format!(" repo:{s}")).unwrap_or_default();
    (
        format!("{base} review-requested:@me{repo}"),
        format!("{base} reviewed-by:@me -author:@me{repo}"),
    )
}

/// Run `gh <args>` in `dir`, returning trimmed stdout. `gh api graphql`
/// exits non-zero on a GraphQL error but still prints the error JSON on
/// stdout, so a failure with JSON on stdout is passed through for
/// [`parse_inbox_response`] to report rather than swallowed.
fn run_gh(dir: &Path, args: &[&str]) -> Result<String, InboxFetchError> {
    let output = match Command::new("gh").args(args).current_dir(dir).output() {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(InboxFetchError::GhNotFound)
        }
        Err(err) => return Err(InboxFetchError::GhFailed(err.to_string())),
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() || stdout.starts_with('{') {
        return Ok(stdout);
    }
    Err(InboxFetchError::GhFailed(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_queries_scope_to_one_repo() {
        let (requested, reviewed) = search_queries(Some("o/r"));
        assert_eq!(
            requested,
            "is:pr is:open archived:false review-requested:@me repo:o/r"
        );
        assert_eq!(
            reviewed,
            "is:pr is:open archived:false reviewed-by:@me -author:@me repo:o/r"
        );
    }

    #[test]
    fn search_queries_without_scope_cover_every_repo() {
        let (requested, reviewed) = search_queries(None);
        assert_eq!(
            requested,
            "is:pr is:open archived:false review-requested:@me"
        );
        assert_eq!(
            reviewed,
            "is:pr is:open archived:false reviewed-by:@me -author:@me"
        );
    }
}
