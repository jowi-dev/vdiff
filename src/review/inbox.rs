//! The pure half of `vdiff --inbox` (issue #36): a review inbox of open PRs
//! that need the viewer's attention, computed fresh from GitHub on every
//! run -- nothing is stored on disk, so running `--inbox` twice with no
//! GitHub changes in between always yields the same list.
//!
//! A PR lands in the inbox for one of three reasons, each one a
//! [`InboxCategory`]:
//!
//! - **review requested**: it matches `review-requested:@me`, which
//!   includes PRs whose author clicked "re-request review".
//! - **new commits**: the viewer has reviewed it, and the PR's head commit
//!   differs from the commit the viewer's latest submitted review was made
//!   on -- the author pushed since the viewer last looked.
//! - **replies**: an unresolved review thread the viewer started has
//!   replies from someone else newer than the viewer's own last comment in
//!   that thread.
//!
//! A PR matching several reasons appears once, under the first category in
//! that order, with every matching reason still recorded on its
//! [`InboxEntry`]. Within a category, entries sort by `updated_at`, most
//! recent first.
//!
//! This module only parses and classifies the response of the single
//! GraphQL query in [`crate::pipeline::inbox`] (the `gh` shell-out half);
//! see `docs/inbox-schema.md` for the `--inbox --json` wire contract.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Why a PR is in the inbox. Serialized in `snake_case`; declaration order
/// is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxCategory {
    ReviewRequested,
    NewCommits,
    Replies,
}

/// One PR in the inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEntry {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub title: String,
    /// The PR author's login; empty for a deleted ("ghost") account.
    pub author: String,
    pub url: String,
    /// ISO-8601 UTC timestamp, as GitHub reports it.
    pub updated_at: String,
    /// The group this entry is shown under (the first matching reason).
    pub category: InboxCategory,
    /// The viewer's review is currently requested.
    pub review_requested: bool,
    /// The PR's head commit differs from the commit of the viewer's latest
    /// submitted review. `false` when the viewer has never reviewed it.
    pub head_changed_since_review: bool,
    /// How many commits were added on top of the reviewed commit. `None`
    /// when the head changed but the reviewed commit is no longer in the
    /// PR's recent history (a force-push or rebase), so no count is known.
    pub new_commits_since_review: Option<u32>,
    /// Replies from others, newer than the viewer's last comment, summed
    /// across unresolved review threads the viewer started.
    pub unanswered_replies: u32,
}

impl InboxEntry {
    /// Why this PR is in the inbox, as one human-readable line listing
    /// every matching reason, e.g. `"3 new commits since your review, 2
    /// replies"`.
    pub fn reason(&self) -> String {
        let mut parts = Vec::new();
        if self.review_requested {
            parts.push("review requested".to_string());
        }
        if self.head_changed_since_review {
            parts.push(match self.new_commits_since_review {
                Some(1) => "1 new commit since your review".to_string(),
                Some(n) => format!("{n} new commits since your review"),
                None => "new commits since your review".to_string(),
            });
        }
        match self.unanswered_replies {
            0 => {}
            1 => parts.push("1 reply".to_string()),
            n => parts.push(format!("{n} replies")),
        }
        parts.join(", ")
    }
}

/// `vdiff --inbox --json`'s output: the whole inbox plus per-category
/// counts, so a status line or board can show a number without walking
/// `entries`. See `docs/inbox-schema.md`.
#[derive(Debug, Clone, Serialize)]
pub struct InboxReport {
    /// The `owner/name` the inbox was limited to, or `null` for
    /// `--all-repos`.
    pub scope: Option<String>,
    pub total: usize,
    pub counts: InboxCounts,
    pub entries: Vec<ReportEntry>,
}

/// Entry count per [`InboxCategory`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InboxCounts {
    pub review_requested: usize,
    pub new_commits: usize,
    pub replies: usize,
}

/// An [`InboxEntry`] plus its [`InboxEntry::reason`] line.
#[derive(Debug, Clone, Serialize)]
pub struct ReportEntry {
    #[serde(flatten)]
    pub entry: InboxEntry,
    pub reason: String,
}

impl InboxReport {
    /// Build the report for `entries` (already sorted, as
    /// [`parse_inbox_response`] returns them) fetched under `scope`.
    pub fn new(scope: Option<String>, entries: Vec<InboxEntry>) -> Self {
        let mut counts = InboxCounts::default();
        for entry in &entries {
            match entry.category {
                InboxCategory::ReviewRequested => counts.review_requested += 1,
                InboxCategory::NewCommits => counts.new_commits += 1,
                InboxCategory::Replies => counts.replies += 1,
            }
        }
        Self {
            scope,
            total: entries.len(),
            counts,
            entries: entries
                .into_iter()
                .map(|entry| ReportEntry {
                    reason: entry.reason(),
                    entry,
                })
                .collect(),
        }
    }
}

/// Everything that can go wrong turning the inbox query's output into
/// entries.
#[derive(Debug, Error)]
pub enum InboxError {
    #[error("couldn't parse the inbox query response: {0}")]
    InvalidResponse(String),
}

/// Parse the inbox GraphQL response (see [`crate::pipeline::inbox`]) for
/// `viewer` into de-duplicated inbox entries, grouped by category and
/// sorted most-recently-updated first within each group. A response
/// carrying GraphQL `errors` and no `data` is an error naming the first
/// message; search nodes that aren't PRs (no `number`) are skipped.
pub fn parse_inbox_response(json: &str, viewer: &str) -> Result<Vec<InboxEntry>, InboxError> {
    let response: raw::Response =
        serde_json::from_str(json).map_err(|err| InboxError::InvalidResponse(err.to_string()))?;
    let Some(data) = response.data else {
        let message = response
            .errors
            .into_iter()
            .next()
            .map(|e| e.message)
            .unwrap_or_else(|| "no data in response".to_string());
        return Err(InboxError::InvalidResponse(message));
    };

    let mut entries: Vec<InboxEntry> = Vec::new();
    let requested = data.requested.nodes.into_iter().map(|n| (n, true));
    let reviewed = data.reviewed.nodes.into_iter().map(|n| (n, false));
    for (node, is_requested) in requested.chain(reviewed) {
        let Some(entry) = classify(node, is_requested, viewer) else {
            continue;
        };
        match entries
            .iter_mut()
            .find(|e| e.repo == entry.repo && e.number == entry.number)
        {
            Some(existing) => {
                existing.review_requested |= entry.review_requested;
                existing.category = existing.category.min(entry.category);
            }
            None => entries.push(entry),
        }
    }
    entries
        .retain(|e| e.review_requested || e.head_changed_since_review || e.unanswered_replies > 0);
    entries.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    Ok(entries)
}

/// Turn one search node into an entry, computing every reason it might be
/// in the inbox. `None` for a non-PR node. The entry's `category` is the
/// first matching reason; an entry matching none is filtered out by the
/// caller.
fn classify(node: raw::PullRequest, is_requested: bool, viewer: &str) -> Option<InboxEntry> {
    let number = node.number?;
    let reviewed_oid = node
        .reviews
        .nodes
        .last()
        .and_then(|review| review.commit.as_ref())
        .map(|commit| commit.oid.as_str());
    let head_changed = reviewed_oid.is_some_and(|oid| oid != node.head_ref_oid);
    let new_commits = match reviewed_oid {
        Some(oid) if head_changed => node
            .commits
            .nodes
            .iter()
            .position(|c| c.commit.oid == oid)
            .map(|idx| (node.commits.nodes.len() - idx - 1) as u32),
        _ => None,
    };
    let replies = node
        .review_threads
        .nodes
        .iter()
        .filter(|t| !t.is_resolved)
        .map(|t| unanswered_in_thread(&t.comments.nodes, viewer))
        .sum();

    let category = if is_requested {
        InboxCategory::ReviewRequested
    } else if head_changed {
        InboxCategory::NewCommits
    } else {
        InboxCategory::Replies
    };
    Some(InboxEntry {
        repo: node.repository.name_with_owner,
        number,
        title: node.title,
        author: login(&node.author).to_string(),
        url: node.url,
        updated_at: node.updated_at,
        category,
        review_requested: is_requested,
        head_changed_since_review: head_changed,
        new_commits_since_review: new_commits,
        unanswered_replies: replies,
    })
}

/// Replies waiting on `viewer` in one thread: zero unless `viewer` wrote
/// its first comment; otherwise the comments by anyone else after
/// `viewer`'s last one.
fn unanswered_in_thread(comments: &[raw::Comment], viewer: &str) -> u32 {
    if comments.first().map(|c| login(&c.author)) != Some(viewer) {
        return 0;
    }
    let last_mine = comments
        .iter()
        .rposition(|c| login(&c.author) == viewer)
        .unwrap_or(0);
    comments[last_mine + 1..]
        .iter()
        .filter(|c| login(&c.author) != viewer)
        .count() as u32
}

/// An actor's login, or `""` for a deleted ("ghost") account.
fn login(actor: &Option<raw::Actor>) -> &str {
    actor.as_ref().map_or("", |a| a.login.as_str())
}

/// The inbox query's response shape, as GitHub's GraphQL API returns it.
/// Every field defaults so a non-PR search node (an empty object) still
/// parses and is then skipped by its missing `number`.
mod raw {
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct Response {
        pub data: Option<Data>,
        #[serde(default)]
        pub errors: Vec<GraphqlError>,
    }

    #[derive(Deserialize)]
    pub struct GraphqlError {
        pub message: String,
    }

    #[derive(Deserialize)]
    pub struct Data {
        pub requested: Connection<PullRequest>,
        pub reviewed: Connection<PullRequest>,
    }

    #[derive(Deserialize)]
    pub struct Connection<T> {
        pub nodes: Vec<T>,
    }

    impl<T> Default for Connection<T> {
        fn default() -> Self {
            Self { nodes: Vec::new() }
        }
    }

    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    pub struct PullRequest {
        pub number: Option<u64>,
        pub title: String,
        pub url: String,
        pub updated_at: String,
        pub author: Option<Actor>,
        pub repository: Repository,
        pub head_ref_oid: String,
        pub reviews: Connection<Review>,
        pub commits: Connection<CommitNode>,
        pub review_threads: Connection<Thread>,
    }

    #[derive(Deserialize)]
    pub struct Actor {
        pub login: String,
    }

    #[derive(Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    pub struct Repository {
        pub name_with_owner: String,
    }

    #[derive(Deserialize)]
    pub struct Review {
        pub commit: Option<Commit>,
    }

    #[derive(Deserialize)]
    pub struct CommitNode {
        pub commit: Commit,
    }

    #[derive(Deserialize)]
    pub struct Commit {
        pub oid: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Thread {
        pub is_resolved: bool,
        pub comments: Connection<Comment>,
    }

    #[derive(Deserialize)]
    pub struct Comment {
        pub author: Option<Actor>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const ME: &str = "me";

    /// A search-result PR node in the inbox query's shape.
    fn pr(number: u64, updated_at: &str) -> Value {
        json!({
            "number": number,
            "title": format!("PR {number}"),
            "url": format!("https://github.com/o/r/pull/{number}"),
            "updatedAt": updated_at,
            "author": { "login": "alice" },
            "repository": { "nameWithOwner": "o/r" },
            "headRefOid": "c3",
            "reviews": { "nodes": [] },
            "commits": { "nodes": [
                { "commit": { "oid": "c1" } },
                { "commit": { "oid": "c2" } },
                { "commit": { "oid": "c3" } }
            ] },
            "reviewThreads": { "nodes": [] }
        })
    }

    fn reviewed_at(mut node: Value, oid: &str) -> Value {
        node["reviews"] = json!({ "nodes": [ { "commit": { "oid": oid } } ] });
        node
    }

    fn with_threads(mut node: Value, threads: Value) -> Value {
        node["reviewThreads"] = json!({ "nodes": threads });
        node
    }

    fn thread(resolved: bool, authors: &[&str]) -> Value {
        let comments: Vec<Value> = authors
            .iter()
            .map(|a| json!({ "author": { "login": a } }))
            .collect();
        json!({ "isResolved": resolved, "comments": { "nodes": comments } })
    }

    fn response(requested: Vec<Value>, reviewed: Vec<Value>) -> String {
        json!({ "data": {
            "requested": { "nodes": requested },
            "reviewed": { "nodes": reviewed }
        } })
        .to_string()
    }

    fn parse(requested: Vec<Value>, reviewed: Vec<Value>) -> Vec<InboxEntry> {
        parse_inbox_response(&response(requested, reviewed), ME).unwrap()
    }

    #[test]
    fn requested_review_is_listed_with_its_fields() {
        let entries = parse(vec![pr(7, "2026-01-01T00:00:00Z")], vec![]);
        assert_eq!(
            entries,
            vec![InboxEntry {
                repo: "o/r".into(),
                number: 7,
                title: "PR 7".into(),
                author: "alice".into(),
                url: "https://github.com/o/r/pull/7".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                category: InboxCategory::ReviewRequested,
                review_requested: true,
                head_changed_since_review: false,
                new_commits_since_review: None,
                unanswered_replies: 0,
            }]
        );
    }

    #[test]
    fn reviewed_pr_with_new_commits_counts_them() {
        let entries = parse(vec![], vec![reviewed_at(pr(3, "t"), "c1")]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].category, InboxCategory::NewCommits);
        assert!(entries[0].head_changed_since_review);
        assert_eq!(entries[0].new_commits_since_review, Some(2));
        assert!(!entries[0].review_requested);
    }

    #[test]
    fn force_pushed_pr_has_changed_head_but_no_count() {
        let entries = parse(vec![], vec![reviewed_at(pr(3, "t"), "gone")]);
        assert_eq!(entries[0].category, InboxCategory::NewCommits);
        assert!(entries[0].head_changed_since_review);
        assert_eq!(entries[0].new_commits_since_review, None);
    }

    #[test]
    fn reviewed_pr_with_nothing_new_is_dropped() {
        assert!(parse(vec![], vec![reviewed_at(pr(3, "t"), "c3")]).is_empty());
    }

    #[test]
    fn replies_count_only_others_after_my_last_comment_on_my_unresolved_threads() {
        let node = with_threads(
            reviewed_at(pr(4, "t"), "c3"),
            json!([
                // Mine, unresolved: bob replied, I answered, carol and bob replied again.
                thread(false, &[ME, "bob", ME, "carol", "bob"]),
                // Mine, unresolved, one reply.
                thread(false, &[ME, "bob"]),
                // Mine but resolved: ignored.
                thread(true, &[ME, "bob"]),
                // Someone else's thread: ignored even though I'm in it.
                thread(false, &["bob", ME, "carol"]),
                // Mine, I had the last word: nothing waiting.
                thread(false, &[ME, "bob", ME]),
            ]),
        );
        let entries = parse(vec![], vec![node]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].category, InboxCategory::Replies);
        assert_eq!(entries[0].unanswered_replies, 3);
        assert!(!entries[0].head_changed_since_review);
    }

    #[test]
    fn deleted_account_replies_count_as_someone_else() {
        let mut t = thread(false, &[ME]);
        t["comments"]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "author": null }));
        let node = with_threads(reviewed_at(pr(4, "t"), "c3"), json!([t]));
        assert_eq!(parse(vec![], vec![node])[0].unanswered_replies, 1);
    }

    #[test]
    fn pr_in_both_searches_appears_once_under_requested_with_all_reasons() {
        let reviewed = reviewed_at(pr(5, "t"), "c2");
        let entries = parse(vec![reviewed.clone()], vec![reviewed]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].category, InboxCategory::ReviewRequested);
        assert!(entries[0].review_requested);
        assert_eq!(entries[0].new_commits_since_review, Some(1));
    }

    #[test]
    fn entries_group_by_category_then_most_recently_updated_first() {
        let entries = parse(
            vec![pr(1, "2026-01-01T00:00:00Z"), pr(2, "2026-03-01T00:00:00Z")],
            vec![
                with_threads(
                    reviewed_at(pr(3, "2026-09-01T00:00:00Z"), "c3"),
                    json!([thread(false, &[ME, "bob"])]),
                ),
                reviewed_at(pr(4, "2026-02-01T00:00:00Z"), "c1"),
                reviewed_at(pr(5, "2026-04-01T00:00:00Z"), "c2"),
            ],
        );
        let order: Vec<u64> = entries.iter().map(|e| e.number).collect();
        assert_eq!(order, vec![2, 1, 5, 4, 3]);
    }

    #[test]
    fn same_number_in_different_repos_are_distinct() {
        let mut other = pr(1, "t");
        other["repository"]["nameWithOwner"] = json!("o/other");
        assert_eq!(parse(vec![pr(1, "t"), other], vec![]).len(), 2);
    }

    #[test]
    fn non_pr_search_nodes_are_skipped() {
        assert!(parse(vec![json!({})], vec![json!({})]).is_empty());
    }

    #[test]
    fn graphql_errors_are_reported() {
        let json = r#"{"errors":[{"message":"Bad credentials"}]}"#;
        let err = parse_inbox_response(json, ME).unwrap_err();
        assert!(err.to_string().contains("Bad credentials"), "{err}");
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse_inbox_response("nope", ME).is_err());
    }

    fn entry() -> InboxEntry {
        parse(vec![pr(7, "2026-01-01T00:00:00Z")], vec![]).remove(0)
    }

    #[test]
    fn reason_lists_every_matching_reason() {
        let mut e = entry();
        assert_eq!(e.reason(), "review requested");
        e.review_requested = false;
        e.head_changed_since_review = true;
        e.new_commits_since_review = Some(1);
        e.unanswered_replies = 1;
        assert_eq!(e.reason(), "1 new commit since your review, 1 reply");
        e.new_commits_since_review = Some(3);
        e.unanswered_replies = 2;
        assert_eq!(e.reason(), "3 new commits since your review, 2 replies");
        e.new_commits_since_review = None;
        e.unanswered_replies = 0;
        assert_eq!(e.reason(), "new commits since your review");
    }

    #[test]
    fn report_serializes_scope_counts_and_entries() {
        let report = InboxReport::new(Some("o/r".into()), vec![entry()]);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["scope"], json!("o/r"));
        assert_eq!(value["total"], json!(1));
        assert_eq!(
            value["counts"],
            json!({ "review_requested": 1, "new_commits": 0, "replies": 0 })
        );
        assert_eq!(value["entries"][0]["category"], json!("review_requested"));
        assert_eq!(value["entries"][0]["reason"], json!("review requested"));
        assert_eq!(value["entries"][0]["new_commits_since_review"], json!(null));

        let all = serde_json::to_value(InboxReport::new(None, vec![])).unwrap();
        assert_eq!(all["scope"], json!(null));
        assert_eq!(all["total"], json!(0));
    }
}
