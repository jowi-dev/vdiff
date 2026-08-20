//! Pure logic backing `vdiff --publish-comments <pr>` (issue #7 phase 2):
//! batch-publishing every locally captured [`Comment`] to a GitHub PR as one
//! review. Everything here is IO-free and unit-tested directly; the glue
//! that actually shells out to `gh` and computes real diff ranges lives in
//! [`crate::pipeline::publish`], and the CLI wiring lives in `main.rs`
//! alongside `--export-comments`.
//!
//! ## Flow
//!
//! 1. Load `comments.json` (never written by this crate -- see
//!    [`crate::review::store`]'s doc).
//! 2. Drop comments already recorded in the sidecar (see [`PublishedStore`])
//!    for this PR, unless `--republish` is given ([`filter_unpublished`]).
//! 3. Partition what's left into line-anchored vs not, given each file's
//!    changed head-line ranges from the PR's base ([`partition_comments`]):
//!    a comment whose range intersects a changed range becomes a GitHub
//!    line comment; everything else lands in the review body under a
//!    "Comments outside the diff" section ([`render_body`]) -- never
//!    dropped, since not every comment needs to sit on a diff line to be
//!    worth surfacing.
//! 4. Build the POST body for `POST /pulls/{n}/reviews`
//!    ([`build_payload`]) -- the modern line/side comment shape, not the
//!    legacy diff-position one.
//! 5. `--dry-run` prints the plan ([`render_plan`]) and stops before any of
//!    this touches `gh`; otherwise the glue side posts it, and only on
//!    success records every published comment's id in the sidecar (via
//!    [`PublishedStore::record`]) so a repeat run skips them next time.
//!
//! The sidecar (`<git_dir>/vdiff/published-comments.json`, IO in
//! [`crate::review::store`]) is vdiff's own bookkeeping -- unlike
//! `comments.json`, vdiff owns writing it outright, since nothing else ever
//! needs to. A comment id is scoped per PR number: publishing to PR 12
//! doesn't stop it from also being published to PR 13.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::comments::Comment;

/// One GitHub review comment in the modern line/side shape (`POST
/// /pulls/{n}/reviews`'s `comments[]`), as opposed to the legacy
/// position-in-diff shape. `start_line`/`start_side` are only present for a
/// multi-line comment -- GitHub itself requires `start_line < line`, so a
/// single-line comment must be posted with just `line` or the API rejects
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhLineComment {
    pub path: String,
    pub line: u32,
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub start_side: Option<String>,
    pub body: String,
}

/// Map `comment` to the shape GitHub's line/side API expects: `line` is
/// always `comment.end_line`; `start_line`/`start_side` are only set when
/// the comment spans more than one line (see [`GhLineComment`]'s doc for
/// why a single-line comment must omit them).
pub fn to_gh_line_comment(comment: &Comment) -> GhLineComment {
    let (start_line, start_side) = if comment.start_line == comment.end_line {
        (None, None)
    } else {
        (Some(comment.start_line), Some("RIGHT".to_string()))
    };
    GhLineComment {
        path: comment.path.clone(),
        line: comment.end_line,
        side: "RIGHT".to_string(),
        start_line,
        start_side,
        body: comment.text.clone(),
    }
}

/// Does `comment`'s `[start_line, end_line]` range (1-based, inclusive)
/// intersect `range` (also 1-based, inclusive)?
fn intersects(comment: &Comment, range: (u32, u32)) -> bool {
    comment.start_line <= range.1 && range.0 <= comment.end_line
}

/// Split `comments` into (line-anchored, body-only), given each file's
/// changed head-line ranges (1-based, inclusive) since the PR's base --
/// see [`crate::pipeline::publish::changed_ranges_for_paths`] for how those
/// are actually computed. A comment whose path has no entry in
/// `changed_ranges` at all (untouched file) or whose range doesn't
/// intersect any of its file's ranges goes to the body half.
pub fn partition_comments(
    comments: &[Comment],
    changed_ranges: &HashMap<String, Vec<(u32, u32)>>,
) -> (Vec<Comment>, Vec<Comment>) {
    let mut line_anchored = Vec::new();
    let mut body_only = Vec::new();
    for comment in comments {
        let eligible = changed_ranges
            .get(&comment.path)
            .map(|ranges| ranges.iter().any(|&range| intersects(comment, range)))
            .unwrap_or(false);
        if eligible {
            line_anchored.push(comment.clone());
        } else {
            body_only.push(comment.clone());
        }
    }
    (line_anchored, body_only)
}

/// `comment`'s range formatted `start` (single line) or `start-end`
/// (multi-line) -- same convention as
/// [`crate::review::comments::render_markdown`].
fn format_range(comment: &Comment) -> String {
    if comment.start_line == comment.end_line {
        format!("{}", comment.start_line)
    } else {
        format!("{}-{}", comment.start_line, comment.end_line)
    }
}

/// The review's top-level `body`: a fixed one-line attribution, plus (only
/// when `body_comments` is non-empty) a "Comments outside the diff"
/// section listing each as `` `path:range` — text``. Never empty, even
/// with no body comments -- GitHub's create-review endpoint expects a
/// non-empty `body` for a `COMMENT`-event review with no line comments.
pub fn render_body(body_comments: &[Comment]) -> String {
    let mut out = String::from("Posted via `vdiff --publish-comments`.\n");
    if !body_comments.is_empty() {
        out.push_str("\n### Comments outside the diff\n\n");
        for comment in body_comments {
            out.push_str(&format!(
                "- `{}:{}` — {}\n",
                comment.path,
                format_range(comment),
                comment.text
            ));
        }
    }
    out
}

/// The full JSON POST body for `POST /pulls/{n}/reviews`: `event:
/// "COMMENT"`, `body`, and `comments` (mapped via [`to_gh_line_comment`]).
pub fn build_payload(line_comments: &[Comment], body: &str) -> serde_json::Value {
    let comments: Vec<GhLineComment> = line_comments.iter().map(to_gh_line_comment).collect();
    serde_json::json!({
        "event": "COMMENT",
        "body": body,
        "comments": comments,
    })
}

/// `--dry-run`'s printed plan: every line comment as `path:range — text`,
/// then the review body that would accompany them. Exactly what would be
/// posted, minus the JSON wire shape.
pub fn render_plan(line_comments: &[Comment], body: &str) -> String {
    let mut out = String::from("Line comments:\n");
    if line_comments.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for comment in line_comments {
            out.push_str(&format!(
                "  {}:{} — {}\n",
                comment.path,
                format_range(comment),
                comment.text
            ));
        }
    }
    out.push_str("\nReview body:\n");
    out.push_str(body);
    out
}

/// One PR a comment has already been published to, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedEntry {
    pub pr: u64,
    pub published_at: String,
}

/// `<git_dir>/vdiff/published-comments.json`'s in-memory shape: which
/// comment ids have been published, and to which PR(s). A [`BTreeMap`] so
/// the serialized JSON always lists comment ids in the same order
/// regardless of insertion order, same rationale as
/// [`crate::review::review_state::BranchReviewState::nodes`]. A comment can
/// be published to more than one PR (e.g. re-targeted after a rebase), so
/// each id maps to a list of [`PublishedEntry`], not a single one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PublishedStore {
    pub comments: BTreeMap<String, Vec<PublishedEntry>>,
}

impl PublishedStore {
    /// Has `comment_id` already been published to `pr`?
    pub fn is_published(&self, comment_id: &str, pr: u64) -> bool {
        self.comments
            .get(comment_id)
            .map(|entries| entries.iter().any(|entry| entry.pr == pr))
            .unwrap_or(false)
    }

    /// Record that `comment_id` was just published to `pr` at
    /// `published_at`, updating the existing entry's timestamp if one for
    /// this `pr` already exists (a `--republish` re-post) rather than
    /// duplicating it. Entries are kept sorted by `pr` so the on-disk JSON
    /// doesn't reorder itself based on republish order.
    pub fn record(&mut self, comment_id: &str, pr: u64, published_at: String) {
        let entries = self.comments.entry(comment_id.to_string()).or_default();
        match entries.iter_mut().find(|entry| entry.pr == pr) {
            Some(existing) => existing.published_at = published_at,
            None => {
                entries.push(PublishedEntry { pr, published_at });
                entries.sort_by_key(|entry| entry.pr);
            }
        }
    }
}

/// `comments` minus every one already published to `pr`, per `store` --
/// unless `republish` is set, in which case nothing is filtered out at all.
pub fn filter_unpublished<'a>(
    comments: &'a [Comment],
    store: &PublishedStore,
    pr: u64,
    republish: bool,
) -> Vec<&'a Comment> {
    comments
        .iter()
        .filter(|comment| republish || !store.is_published(&comment.id, pr))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(path: &str, start: u32, end: u32) -> Comment {
        Comment {
            id: "c1".to_string(),
            path: path.to_string(),
            start_line: start,
            end_line: end,
            text: "some text".to_string(),
            node: None,
            created_at: "2026-08-18T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn to_gh_line_comment_single_line_omits_start_fields() {
        let c = comment("src/lib.rs", 5, 5);
        let gh = to_gh_line_comment(&c);
        assert_eq!(gh.line, 5);
        assert_eq!(gh.side, "RIGHT");
        assert_eq!(gh.start_line, None);
        assert_eq!(gh.start_side, None);
    }

    #[test]
    fn to_gh_line_comment_multi_line_sets_start_fields() {
        let c = comment("src/lib.rs", 5, 8);
        let gh = to_gh_line_comment(&c);
        assert_eq!(gh.line, 8, "line is always end_line");
        assert_eq!(gh.start_line, Some(5));
        assert_eq!(gh.start_side, Some("RIGHT".to_string()));
    }

    #[test]
    fn to_gh_line_comment_serializes_without_start_fields_when_absent() {
        let c = comment("src/lib.rs", 5, 5);
        let json = serde_json::to_string(&to_gh_line_comment(&c)).unwrap();
        assert!(!json.contains("start_line"), "json: {json}");
        assert!(!json.contains("start_side"), "json: {json}");
    }

    fn ranges(pairs: Vec<(&str, Vec<(u32, u32)>)>) -> HashMap<String, Vec<(u32, u32)>> {
        pairs
            .into_iter()
            .map(|(path, r)| (path.to_string(), r))
            .collect()
    }

    #[test]
    fn partition_comments_inside_a_changed_range_is_line_anchored() {
        let comments = vec![comment("src/lib.rs", 3, 3)];
        let changed = ranges(vec![("src/lib.rs", vec![(1, 5)])]);
        let (line, body) = partition_comments(&comments, &changed);
        assert_eq!(line.len(), 1);
        assert!(body.is_empty());
    }

    #[test]
    fn partition_comments_untouched_file_goes_to_body() {
        let comments = vec![comment("src/untouched.rs", 3, 3)];
        let changed = ranges(vec![("src/lib.rs", vec![(1, 5)])]);
        let (line, body) = partition_comments(&comments, &changed);
        assert!(line.is_empty());
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn partition_comments_empty_diff_for_file_goes_to_body() {
        let comments = vec![comment("src/lib.rs", 3, 3)];
        let changed = ranges(vec![("src/lib.rs", vec![])]);
        let (line, body) = partition_comments(&comments, &changed);
        assert!(line.is_empty());
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn partition_comments_outside_every_range_goes_to_body() {
        let comments = vec![comment("src/lib.rs", 10, 10)];
        let changed = ranges(vec![("src/lib.rs", vec![(1, 5), (20, 25)])]);
        let (line, body) = partition_comments(&comments, &changed);
        assert!(line.is_empty());
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn partition_comments_touching_range_lower_boundary_is_anchored() {
        // Comment's end_line == range's start -- one line of overlap.
        let comments = vec![comment("src/lib.rs", 1, 5)];
        let changed = ranges(vec![("src/lib.rs", vec![(5, 10)])]);
        let (line, body) = partition_comments(&comments, &changed);
        assert_eq!(line.len(), 1);
        assert!(body.is_empty());
    }

    #[test]
    fn partition_comments_touching_range_upper_boundary_is_anchored() {
        // Comment's start_line == range's end -- one line of overlap.
        let comments = vec![comment("src/lib.rs", 10, 15)];
        let changed = ranges(vec![("src/lib.rs", vec![(5, 10)])]);
        let (line, body) = partition_comments(&comments, &changed);
        assert_eq!(line.len(), 1);
        assert!(body.is_empty());
    }

    #[test]
    fn partition_comments_one_line_past_boundary_is_not_anchored() {
        let comments = vec![comment("src/lib.rs", 11, 15)];
        let changed = ranges(vec![("src/lib.rs", vec![(5, 10)])]);
        let (line, body) = partition_comments(&comments, &changed);
        assert!(line.is_empty());
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn render_body_omits_section_when_no_body_comments() {
        let body = render_body(&[]);
        assert!(!body.contains("Comments outside the diff"));
        assert!(!body.is_empty(), "body must never be empty");
    }

    #[test]
    fn render_body_lists_each_body_comment() {
        let comments = vec![comment("src/lib.rs", 1, 1), comment("src/other.rs", 4, 6)];
        let body = render_body(&comments);
        assert!(body.contains("Comments outside the diff"));
        assert!(body.contains("src/lib.rs:1"));
        assert!(body.contains("src/other.rs:4-6"));
    }

    #[test]
    fn build_payload_has_comment_event_and_both_halves() {
        let line_comments = vec![comment("src/lib.rs", 1, 1)];
        let payload = build_payload(&line_comments, "hello");
        assert_eq!(payload["event"], "COMMENT");
        assert_eq!(payload["body"], "hello");
        assert_eq!(payload["comments"].as_array().unwrap().len(), 1);
        assert_eq!(payload["comments"][0]["path"], "src/lib.rs");
    }

    #[test]
    fn render_plan_lists_none_when_no_line_comments() {
        let plan = render_plan(&[], "body text");
        assert!(plan.contains("(none)"));
        assert!(plan.contains("body text"));
    }

    #[test]
    fn render_plan_lists_each_line_comment() {
        let line_comments = vec![comment("src/lib.rs", 2, 2)];
        let plan = render_plan(&line_comments, "body text");
        assert!(plan.contains("src/lib.rs:2"));
        assert!(plan.contains("some text"));
    }

    #[test]
    fn published_store_round_trips_through_json() {
        let mut store = PublishedStore::default();
        store.record("c1", 12, "2026-08-18T00:00:00Z".to_string());
        let json = serde_json::to_string_pretty(&store).unwrap();
        let back: PublishedStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, store);
    }

    #[test]
    fn published_store_record_then_is_published() {
        let mut store = PublishedStore::default();
        assert!(!store.is_published("c1", 12));
        store.record("c1", 12, "2026-08-18T00:00:00Z".to_string());
        assert!(store.is_published("c1", 12));
    }

    #[test]
    fn published_store_scoped_per_pr() {
        let mut store = PublishedStore::default();
        store.record("c1", 12, "2026-08-18T00:00:00Z".to_string());
        assert!(store.is_published("c1", 12));
        assert!(
            !store.is_published("c1", 13),
            "publishing to PR 12 shouldn't count as published to PR 13"
        );
    }

    #[test]
    fn published_store_record_updates_timestamp_not_duplicate_entry() {
        let mut store = PublishedStore::default();
        store.record("c1", 12, "2026-08-18T00:00:00Z".to_string());
        store.record("c1", 12, "2026-08-19T00:00:00Z".to_string());
        assert_eq!(store.comments["c1"].len(), 1);
        assert_eq!(store.comments["c1"][0].published_at, "2026-08-19T00:00:00Z");
    }

    #[test]
    fn filter_unpublished_skips_already_published_comments() {
        let mut store = PublishedStore::default();
        store.record("c1", 12, "2026-08-18T00:00:00Z".to_string());
        let mut c1 = comment("src/lib.rs", 1, 1);
        c1.id = "c1".to_string();
        let mut c2 = comment("src/lib.rs", 2, 2);
        c2.id = "c2".to_string();
        let comments = vec![c1, c2];

        let to_publish = filter_unpublished(&comments, &store, 12, false);
        assert_eq!(to_publish.len(), 1);
        assert_eq!(to_publish[0].id, "c2");
    }

    #[test]
    fn filter_unpublished_allows_republish_to_a_different_pr() {
        let mut store = PublishedStore::default();
        store.record("c1", 12, "2026-08-18T00:00:00Z".to_string());
        let mut c1 = comment("src/lib.rs", 1, 1);
        c1.id = "c1".to_string();
        let comments = vec![c1];

        let to_publish = filter_unpublished(&comments, &store, 13, false);
        assert_eq!(to_publish.len(), 1, "PR 13 hasn't seen this comment yet");
    }

    #[test]
    fn filter_unpublished_with_republish_ignores_sidecar() {
        let mut store = PublishedStore::default();
        store.record("c1", 12, "2026-08-18T00:00:00Z".to_string());
        let mut c1 = comment("src/lib.rs", 1, 1);
        c1.id = "c1".to_string();
        let comments = vec![c1];

        let to_publish = filter_unpublished(&comments, &store, 12, true);
        assert_eq!(to_publish.len(), 1, "--republish posts everything again");
    }
}
