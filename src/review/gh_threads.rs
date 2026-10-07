//! Pure data model for GitHub PR review threads (issue #35): the
//! [`ReviewThread`]/[`ReviewSummary`] shapes, parsing `gh api graphql`'s
//! `pullRequest.reviewThreads` response, attaching threads to graph nodes
//! (by file path, the same rule [`super::comments::map_comments`] uses for
//! local comments), and the ordering/formatting the thread list panel
//! shows. Zero IO -- the `gh` shellout lives in
//! [`crate::pipeline::gh_threads`].
//!
//! GitHub is the source of truth: threads are only ever held in memory and
//! re-fetched on refresh, never cached to disk. The resolved/outdated flags
//! only exist on GitHub's GraphQL API, which is why this parses GraphQL
//! rather than the REST review-comments endpoint.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::graph::model::{NodeId, ProjectGraph};

/// The GraphQL query [`parse_threads_response`] expects the output of.
/// Takes `$owner`, `$name`, and `$number`. Fetches at most 100 threads,
/// 100 reviews, and 100 comments per thread -- no pagination, which covers
/// every PR a human reviews in one sitting.
pub const THREADS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number
      headRefOid
      reviews(first: 100) { nodes { author { login } state body } }
      reviewThreads(first: 100) {
        nodes {
          id path line isResolved isOutdated
          comments(first: 100) { nodes { author { login } body } }
        }
      }
    }
  }
}";

/// GitHub's display name for an account that no longer exists -- what the
/// web UI shows when a comment's `author` comes back `null`.
const GHOST: &str = "ghost";

/// One comment inside a [`ReviewThread`]: the thread's opening comment is
/// `comments[0]`, everything after it a reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadComment {
    pub author: String,
    pub body: String,
}

/// A GitHub review thread: an anchored comment on a file and line, plus its
/// replies, with GitHub's resolved and outdated flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    /// GitHub's GraphQL node id for the thread.
    pub id: String,
    /// Repo-relative path the thread is anchored to.
    pub path: String,
    /// 1-based line on the PR's head commit, or `None` when GitHub can no
    /// longer place the thread on the current diff.
    pub line: Option<u32>,
    pub is_resolved: bool,
    /// Later commits changed the lines this thread was anchored to.
    pub is_outdated: bool,
    pub comments: Vec<ThreadComment>,
}

/// A top-level review summary: a review's own body, with no file anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub author: String,
    /// GitHub's review state: `APPROVED`, `CHANGES_REQUESTED`,
    /// `COMMENTED`, `DISMISSED`, or `PENDING`.
    pub state: String,
    pub body: String,
}

/// Everything fetched for one PR.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PrThreads {
    pub pr_number: u64,
    /// The PR's head commit: thread lines are relative to it, so a local
    /// HEAD that differs means the lines may have drifted.
    pub head_oid: String,
    pub threads: Vec<ReviewThread>,
    /// Only reviews with a non-empty body: a review that only carries line
    /// comments has nothing to say at the top level.
    pub summaries: Vec<ReviewSummary>,
}

/// Wire shapes for [`THREADS_QUERY`]'s response, kept private so the
/// public types above stay free of GraphQL's nesting.
mod raw {
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct Response {
        pub data: Option<Data>,
        #[serde(default)]
        pub errors: Vec<Error>,
    }
    #[derive(Deserialize)]
    pub struct Error {
        pub message: String,
    }
    #[derive(Deserialize)]
    pub struct Data {
        pub repository: Option<Repository>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Repository {
        pub pull_request: Option<PullRequest>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PullRequest {
        pub number: u64,
        pub head_ref_oid: String,
        pub reviews: Nodes<Review>,
        pub review_threads: Nodes<Thread>,
    }
    #[derive(Deserialize)]
    pub struct Nodes<T> {
        pub nodes: Vec<T>,
    }
    #[derive(Deserialize)]
    pub struct Author {
        pub login: String,
    }
    #[derive(Deserialize)]
    pub struct Review {
        pub author: Option<Author>,
        pub state: String,
        pub body: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Thread {
        pub id: String,
        pub path: String,
        pub line: Option<u32>,
        pub is_resolved: bool,
        pub is_outdated: bool,
        pub comments: Nodes<Comment>,
    }
    #[derive(Deserialize)]
    pub struct Comment {
        pub author: Option<Author>,
        pub body: String,
    }
}

fn author_login(author: Option<raw::Author>) -> String {
    author.map_or_else(|| GHOST.to_string(), |a| a.login)
}

/// Parse `gh api graphql`'s output for [`THREADS_QUERY`]. A GraphQL-level
/// error (unknown PR, missing permission) comes back as an `Err` carrying
/// GitHub's own messages, which already read as user-facing text.
pub fn parse_threads_response(json: &str) -> Result<PrThreads, String> {
    let response: raw::Response = serde_json::from_str(json)
        .map_err(|err| format!("couldn't parse GraphQL output: {err}"))?;
    if !response.errors.is_empty() {
        let messages: Vec<String> = response.errors.into_iter().map(|e| e.message).collect();
        return Err(messages.join("; "));
    }
    let pr = response
        .data
        .and_then(|d| d.repository)
        .and_then(|r| r.pull_request)
        .ok_or_else(|| "GraphQL response had no pull request".to_string())?;
    Ok(PrThreads {
        pr_number: pr.number,
        head_oid: pr.head_ref_oid,
        threads: pr
            .review_threads
            .nodes
            .into_iter()
            .map(|t| ReviewThread {
                id: t.id,
                path: t.path,
                line: t.line,
                is_resolved: t.is_resolved,
                is_outdated: t.is_outdated,
                comments: t
                    .comments
                    .nodes
                    .into_iter()
                    .map(|c| ThreadComment {
                        author: author_login(c.author),
                        body: c.body,
                    })
                    .collect(),
            })
            .collect(),
        summaries: pr
            .reviews
            .nodes
            .into_iter()
            .filter(|r| !r.body.trim().is_empty())
            .map(|r| ReviewSummary {
                author: author_login(r.author),
                state: r.state,
                body: r.body,
            })
            .collect(),
    })
}

/// The line to place `thread` at inline in a file, or `None` for an
/// outdated thread: it no longer points at a line in the current diff, so
/// it belongs in the thread list only.
pub fn inline_line(thread: &ReviewThread) -> Option<u32> {
    if thread.is_outdated {
        None
    } else {
        thread.line
    }
}

/// Every node whose files contain `thread.path`, sorted -- the same path
/// rule [`super::comments::map_comments`] applies to local comments.
pub fn thread_nodes(graph: &ProjectGraph, thread: &ReviewThread) -> Vec<NodeId> {
    let mut ids: Vec<NodeId> = graph
        .nodes
        .values()
        .filter(|node| {
            node.files
                .iter()
                .any(|f| f.path.to_string_lossy() == thread.path)
        })
        .map(|node| node.id.clone())
        .collect();
    ids.sort();
    ids
}

/// Unresolved thread count per node, for the graph's GitHub-thread badge.
/// A node with only resolved threads is absent rather than mapped to `0`.
pub fn unresolved_counts(graph: &ProjectGraph, threads: &[ReviewThread]) -> HashMap<NodeId, usize> {
    let mut counts = HashMap::new();
    for thread in threads.iter().filter(|t| !t.is_resolved) {
        for id in thread_nodes(graph, thread) {
            *counts.entry(id).or_insert(0) += 1;
        }
    }
    counts
}

/// One row of the thread list panel: an index into
/// [`PrThreads::summaries`] or [`PrThreads::threads`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadEntry {
    Summary(usize),
    Thread(usize),
}

/// The thread list's row order: review summaries first (they have no node
/// and frame everything below), then unresolved threads, then resolved
/// ones, each group sorted by path and line.
pub fn list_entries(threads: &PrThreads) -> Vec<ThreadEntry> {
    let mut thread_order: Vec<usize> = (0..threads.threads.len()).collect();
    thread_order.sort_by(|&a, &b| {
        let (a, b) = (&threads.threads[a], &threads.threads[b]);
        (a.is_resolved, &a.path, a.line).cmp(&(b.is_resolved, &b.path, b.line))
    });
    (0..threads.summaries.len())
        .map(ThreadEntry::Summary)
        .chain(thread_order.into_iter().map(ThreadEntry::Thread))
        .collect()
}

/// The first non-blank line of `body`, trimmed.
fn first_line(body: &str) -> &str {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// One-line rendering of `entry` for the thread list: location, author,
/// the first line of the body, reply count, and resolved/outdated state.
pub fn format_entry(threads: &PrThreads, entry: ThreadEntry) -> String {
    match entry {
        ThreadEntry::Summary(i) => {
            let s = &threads.summaries[i];
            format!(
                "review  @{}  {}  [{}]",
                s.author,
                first_line(&s.body),
                s.state.to_lowercase().replace('_', " ")
            )
        }
        ThreadEntry::Thread(i) => {
            let t = &threads.threads[i];
            let location = match inline_line(t) {
                Some(line) => format!("{}:{line}", t.path),
                None => t.path.clone(),
            };
            let (author, body) = t
                .comments
                .first()
                .map_or(("", ""), |c| (c.author.as_str(), first_line(&c.body)));
            let mut out = format!("{location}  @{author}  {body}");
            match t.comments.len().saturating_sub(1) {
                0 => {}
                1 => out.push_str("  (+1 reply)"),
                n => out.push_str(&format!("  (+{n} replies)")),
            }
            if t.is_resolved {
                out.push_str("  [resolved]");
            } else if t.is_outdated {
                out.push_str("  [outdated]");
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{FileRef, GitStatus, ModuleNode};
    use std::path::PathBuf;

    const RESPONSE: &str = r#"{"data":{"repository":{"pullRequest":{
        "number": 35,
        "headRefOid": "abc123",
        "reviews": {"nodes": [
            {"author": {"login": "rev"}, "state": "CHANGES_REQUESTED", "body": "Needs work.\nSee threads."},
            {"author": {"login": "rev"}, "state": "COMMENTED", "body": ""}
        ]},
        "reviewThreads": {"nodes": [
            {"id": "T1", "path": "src/a.rs", "line": 10, "isResolved": true, "isOutdated": false,
             "comments": {"nodes": [{"author": {"login": "bob"}, "body": "nit"}]}},
            {"id": "T2", "path": "src/b.rs", "line": null, "isResolved": false, "isOutdated": true,
             "comments": {"nodes": [{"author": null, "body": "old"}]}},
            {"id": "T3", "path": "src/a.rs", "line": 4, "isResolved": false, "isOutdated": false,
             "comments": {"nodes": [
                {"author": {"login": "bob"}, "body": "why this?\nmore detail"},
                {"author": {"login": "me"}, "body": "because"}
             ]}}
        ]}
    }}}}"#;

    fn parsed() -> PrThreads {
        parse_threads_response(RESPONSE).unwrap()
    }

    #[test]
    fn query_asks_for_resolution_flags_and_head() {
        for field in [
            "reviewThreads",
            "isResolved",
            "isOutdated",
            "headRefOid",
            "reviews",
        ] {
            assert!(THREADS_QUERY.contains(field), "query missing {field}");
        }
    }

    #[test]
    fn parse_reads_pr_number_head_and_threads() {
        let t = parsed();
        assert_eq!(t.pr_number, 35);
        assert_eq!(t.head_oid, "abc123");
        assert_eq!(t.threads.len(), 3);
        let t3 = &t.threads[2];
        assert_eq!(t3.id, "T3");
        assert_eq!(t3.path, "src/a.rs");
        assert_eq!(t3.line, Some(4));
        assert!(!t3.is_resolved);
        assert_eq!(t3.comments.len(), 2);
        assert_eq!(t3.comments[1].author, "me");
    }

    #[test]
    fn parse_maps_deleted_author_to_ghost() {
        assert_eq!(parsed().threads[1].comments[0].author, "ghost");
    }

    #[test]
    fn parse_keeps_only_reviews_with_a_body() {
        let t = parsed();
        assert_eq!(t.summaries.len(), 1);
        assert_eq!(t.summaries[0].state, "CHANGES_REQUESTED");
    }

    #[test]
    fn parse_surfaces_graphql_errors() {
        let err = parse_threads_response(
            r#"{"data":null,"errors":[{"message":"Could not resolve to a PullRequest"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("Could not resolve"), "{err}");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_threads_response("not json").is_err());
    }

    #[test]
    fn inline_line_skips_outdated_threads() {
        let t = parsed();
        assert_eq!(inline_line(&t.threads[2]), Some(4));
        assert_eq!(inline_line(&t.threads[1]), None);
        let mut outdated_with_line = t.threads[2].clone();
        outdated_with_line.is_outdated = true;
        assert_eq!(inline_line(&outdated_with_line), None);
    }

    fn node(id: &str, path: &str) -> ModuleNode {
        ModuleNode {
            id: NodeId::from(id),
            display_name: id.to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Modified,
            files: vec![FileRef {
                path: PathBuf::from(path),
                base_blob: None,
                head_blob: Some("h".to_string()),
            }],
        }
    }

    fn graph() -> ProjectGraph {
        let nodes = vec![node("a", "src/a.rs"), node("b", "src/b.rs")];
        ProjectGraph {
            roots: nodes.iter().map(|n| n.id.clone()).collect(),
            nodes: nodes.into_iter().map(|n| (n.id.clone(), n)).collect(),
            edges: vec![],
        }
    }

    #[test]
    fn thread_nodes_matches_by_path() {
        let t = parsed();
        assert_eq!(
            thread_nodes(&graph(), &t.threads[0]),
            vec![NodeId::from("a")]
        );
        let mut stray = t.threads[0].clone();
        stray.path = "elsewhere.rs".to_string();
        assert!(thread_nodes(&graph(), &stray).is_empty());
    }

    #[test]
    fn unresolved_counts_skip_resolved_threads() {
        let counts = unresolved_counts(&graph(), &parsed().threads);
        assert_eq!(counts.get(&NodeId::from("a")), Some(&1));
        assert_eq!(counts.get(&NodeId::from("b")), Some(&1));
    }

    #[test]
    fn list_puts_summaries_first_then_unresolved_then_resolved() {
        assert_eq!(
            list_entries(&parsed()),
            vec![
                ThreadEntry::Summary(0),
                ThreadEntry::Thread(2),
                ThreadEntry::Thread(1),
                ThreadEntry::Thread(0),
            ]
        );
    }

    #[test]
    fn format_thread_entry_shows_location_author_first_line_replies_state() {
        let t = parsed();
        assert_eq!(
            format_entry(&t, ThreadEntry::Thread(2)),
            "src/a.rs:4  @bob  why this?  (+1 reply)"
        );
        assert_eq!(
            format_entry(&t, ThreadEntry::Thread(0)),
            "src/a.rs:10  @bob  nit  [resolved]"
        );
        assert_eq!(
            format_entry(&t, ThreadEntry::Thread(1)),
            "src/b.rs  @ghost  old  [outdated]"
        );
    }

    #[test]
    fn format_summary_entry_shows_review_state() {
        assert_eq!(
            format_entry(&parsed(), ThreadEntry::Summary(0)),
            "review  @rev  Needs work.  [changes requested]"
        );
    }
}
