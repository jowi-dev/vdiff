//! Pure data model for local review comments: the [`Comment`] struct, its
//! serde round-trip, sorting/id-generation helpers, markdown rendering for
//! `--export-comments`, and [`map_comments`], which attaches comments to
//! graph nodes for the graph's comment badge (issue #14). Zero IO --
//! loading/saving `comments.json` lives in [`crate::review::store`], the
//! thin glue-side wrapper around this module. See the `review` module doc
//! for the feature this backs.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::graph::model::{NodeId, ProjectGraph};

/// One review comment, either anchored to a line range in a file (captured
/// via `:VdiffComment` inside the embedded nvim pane) or to a node as a
/// whole (an "architecture" comment, captured via the graph pane's `c` key
/// -- see [`Self::node`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// A short, stable identifier (`"c1"`, `"c2"`, ... -- see
    /// [`next_id`]), so a comment can be referenced (e.g. for manual
    /// deletion by hand-editing the JSON) without relying on array
    /// position.
    pub id: String,
    /// Repo-relative path the comment is anchored to.
    pub path: String,
    /// 1-based, inclusive: the first line of the commented range in the
    /// current worktree/head version of the file.
    pub start_line: u32,
    /// 1-based, inclusive: the last line of the commented range. Equal to
    /// `start_line` for a single-line comment.
    pub end_line: u32,
    /// The comment's body. Multi-line is allowed (the MVP compose UX is
    /// single-line only -- see the module doc on the nvim side -- but the
    /// model itself doesn't assume that).
    pub text: String,
    /// The graph node this comment is about, if it was captured as an
    /// "architecture" comment from the graph pane (`c` on a focused node)
    /// rather than a visual selection inside nvim.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub node: Option<String>,
    /// ISO-8601 UTC timestamp (see [`format_iso8601`]), set once at
    /// creation.
    pub created_at: String,
}

/// Sort `comments` by `path`, then `start_line` -- the stable ordering the
/// on-disk JSON is always written in, so a `git diff` of `comments.json`
/// stays sane (new comments land near their neighbors, not appended at the
/// end in whatever order they were captured).
pub fn sort_comments(comments: &mut [Comment]) {
    comments.sort_by(|a, b| a.path.cmp(&b.path).then(a.start_line.cmp(&b.start_line)));
}

/// Append `comment` to `comments` and re-sort -- the one mutating entry
/// point every caller should use rather than pushing directly, so the
/// sort-after-insert invariant can never be forgotten at a call site.
pub fn add_comment(comments: &mut Vec<Comment>, comment: Comment) {
    comments.push(comment);
    sort_comments(comments);
}

/// The next id to assign: `"c<n>"` where `n` is one more than the highest
/// numeric suffix already in use among ids of the form `c<digits>` (`0` if
/// there are none, or if every existing id has some other shape -- a
/// hand-edited id, say). Deterministic and free of any ambient
/// randomness/clock, so a fresh id never collides with an existing one as
/// long as callers always go through [`add_comment`] first... though
/// nothing stops a hand-edited `comments.json` from introducing a
/// duplicate; ids are a convenience for referencing comments, not a
/// uniqueness-enforced primary key.
pub fn next_id(existing: &[Comment]) -> String {
    let max = existing
        .iter()
        .filter_map(|c| c.id.strip_prefix('c'))
        .filter_map(|digits| digits.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("c{}", max + 1)
}

/// Format `time` as an ISO-8601 UTC timestamp with second precision
/// (`2026-08-18T10:15:30Z`). Hand-rolled rather than pulling in `chrono`/
/// `time` for one formatter -- `time` is already a transitive dep signature
/// vdiff doesn't otherwise need. Pure given the `SystemTime` value (never
/// calls `SystemTime::now()` itself -- that's the glue side's job, at the
/// one call site that actually creates a [`Comment`]).
pub fn format_iso8601(time: SystemTime) -> String {
    let secs = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let (year, month, day, hour, minute, second) = civil_from_unix_seconds(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a Unix timestamp (seconds since the epoch, UTC) into
/// `(year, month, day, hour, minute, second)` using Howard Hinnant's
/// `civil_from_days` algorithm (proleptic Gregorian, correct for any
/// non-negative day count including leap years/centuries) -- the standard
/// division-free trick for this without a calendar library.
fn civil_from_unix_seconds(total_secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (total_secs / 86400) as i64;
    let rem = total_secs % 86400;
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;
    let second = (rem % 60) as u32;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    (year, month, day, hour, minute, second)
}

/// Render `comments` (assumed already [`sort_comments`]-ordered -- callers
/// load from disk, which is always saved sorted) as the markdown
/// `--export-comments` prints: a header naming `repo`/`branch`, then one
/// `### path:start[-end]` section per comment (an `(node: ...)` suffix when
/// present), the comment's text below it. Empty `comments` still gets the
/// header, followed by a "No comments." line, and exit-0 is the CLI's job
/// (this function has no exit-code concept).
pub fn render_markdown(comments: &[Comment], repo: &str, branch: &str) -> String {
    let mut out = format!("# vdiff review comments — {repo} @ {branch}\n\n");
    if comments.is_empty() {
        out.push_str("No comments.\n");
        return out;
    }
    for comment in comments {
        let range = if comment.start_line == comment.end_line {
            format!("{}", comment.start_line)
        } else {
            format!("{}-{}", comment.start_line, comment.end_line)
        };
        let node_suffix = comment
            .node
            .as_ref()
            .map(|node| format!(" (node: {node})"))
            .unwrap_or_default();
        out.push_str(&format!(
            "### {}:{}{}\n\n{}\n\n",
            comment.path, range, node_suffix, comment.text
        ));
    }
    out
}

/// Attach every comment in `comments` to the node(s) it's about, same
/// spirit as [`crate::review::findings::map_findings`]: a comment with
/// [`Comment::node`] set attaches directly to that node (an unknown id --
/// the comment predates the current graph shape -- is skipped silently,
/// not reported anywhere, since unlike a findings agent there's no "just
/// ran this" contract to warn a human about), and `path` attaches to every
/// node whose [`crate::graph::model::FileRef::path`] matches it (one file
/// can back more than one node -- see [`crate::graph::model::FileRef`]'s
/// doc). When both `node` and `path` resolve to the same node, that node
/// gets the comment exactly once, never twice.
pub fn map_comments(graph: &ProjectGraph, comments: &[Comment]) -> HashMap<NodeId, Vec<Comment>> {
    let mut result: HashMap<NodeId, Vec<Comment>> = HashMap::new();
    for comment in comments {
        for target in resolve_targets(graph, comment) {
            result.entry(target).or_default().push(comment.clone());
        }
    }
    result
}

/// The node id(s) `comment` resolves to in `graph`: `node` directly (if it
/// names a node that exists in `graph`), plus every node whose `files`
/// contains `path` -- deduplicated so a comment whose `node` and `path`
/// both point at the same node only appears once in the result.
fn resolve_targets(graph: &ProjectGraph, comment: &Comment) -> Vec<NodeId> {
    let mut targets = Vec::new();
    if let Some(node_id) = &comment.node {
        let id = NodeId::from(node_id.clone());
        if graph.node(&id).is_some() {
            targets.push(id);
        }
    }
    for node in graph.nodes.values() {
        if node.files.iter().any(|f| f.path.to_string_lossy() == comment.path)
            && !targets.contains(&node.id)
        {
            targets.push(node.id.clone());
        }
    }
    targets
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
    fn serde_round_trip_preserves_fields() {
        let mut c = comment("src/lib.rs", 3, 5);
        c.node = Some("rust:crate".to_string());
        let json = serde_json::to_string(&c).expect("serialize");
        let back: Comment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(c, back);
    }

    #[test]
    fn serde_omits_absent_node_field() {
        let c = comment("src/lib.rs", 1, 1);
        let json = serde_json::to_string(&c).expect("serialize");
        assert!(
            !json.contains("\"node\""),
            "node key should be omitted when None: {json}"
        );
    }

    #[test]
    fn sort_comments_orders_by_path_then_start_line() {
        let mut comments = vec![
            comment("b.rs", 10, 10),
            comment("a.rs", 5, 5),
            comment("a.rs", 1, 1),
        ];
        sort_comments(&mut comments);
        let paths_and_lines: Vec<(String, u32)> = comments
            .iter()
            .map(|c| (c.path.clone(), c.start_line))
            .collect();
        assert_eq!(
            paths_and_lines,
            vec![
                ("a.rs".to_string(), 1),
                ("a.rs".to_string(), 5),
                ("b.rs".to_string(), 10),
            ]
        );
    }

    #[test]
    fn add_comment_inserts_in_sorted_position() {
        let mut comments = vec![comment("a.rs", 1, 1), comment("c.rs", 1, 1)];
        add_comment(&mut comments, comment("b.rs", 1, 1));
        let paths: Vec<&str> = comments.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.rs", "c.rs"]);
    }

    #[test]
    fn next_id_starts_at_c1_when_empty() {
        assert_eq!(next_id(&[]), "c1");
    }

    #[test]
    fn next_id_increments_past_highest_existing_numeric_suffix() {
        let mut a = comment("a.rs", 1, 1);
        a.id = "c3".to_string();
        let mut b = comment("a.rs", 2, 2);
        b.id = "c1".to_string();
        assert_eq!(next_id(&[a, b]), "c4");
    }

    #[test]
    fn next_id_ignores_non_numeric_hand_edited_ids() {
        let mut a = comment("a.rs", 1, 1);
        a.id = "renamed-by-hand".to_string();
        assert_eq!(next_id(&[a]), "c1");
    }

    #[test]
    fn format_iso8601_known_epoch_values() {
        assert_eq!(
            format_iso8601(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(
            format_iso8601(SystemTime::UNIX_EPOCH + Duration::from_secs(1_755_511_530)),
            "2025-08-18T10:05:30Z"
        );
    }

    #[test]
    fn render_markdown_empty_says_no_comments() {
        let out = render_markdown(&[], "vdiff", "main");
        assert!(out.contains("vdiff @ main"));
        assert!(out.contains("No comments."));
    }

    #[test]
    fn render_markdown_single_line_range_omits_dash() {
        let out = render_markdown(&[comment("src/lib.rs", 12, 12)], "vdiff", "main");
        assert!(out.contains("### src/lib.rs:12\n"));
        assert!(!out.contains("12-12"));
    }

    #[test]
    fn render_markdown_multi_line_range_shows_dash() {
        let out = render_markdown(&[comment("src/lib.rs", 12, 14)], "vdiff", "main");
        assert!(out.contains("### src/lib.rs:12-14\n"));
    }

    #[test]
    fn render_markdown_includes_node_suffix_when_present() {
        let mut c = comment("src/lib.rs", 1, 1);
        c.node = Some("rust:crate".to_string());
        let out = render_markdown(&[c], "vdiff", "main");
        assert!(out.contains("### src/lib.rs:1 (node: rust:crate)\n"));
    }

    #[test]
    fn render_markdown_omits_node_suffix_when_absent() {
        let out = render_markdown(&[comment("src/lib.rs", 1, 1)], "vdiff", "main");
        assert!(out.contains("### src/lib.rs:1\n"));
        assert!(!out.contains("node:"));
    }

    #[test]
    fn render_markdown_preserves_multi_line_text() {
        let mut c = comment("src/lib.rs", 1, 1);
        c.text = "line one\nline two".to_string();
        let out = render_markdown(&[c], "vdiff", "main");
        assert!(out.contains("line one\nline two"));
    }

    #[test]
    fn render_markdown_groups_multiple_files_in_sorted_order() {
        let comments = vec![comment("a.rs", 1, 1), comment("b.rs", 1, 1)];
        let out = render_markdown(&comments, "vdiff", "main");
        let a_pos = out.find("a.rs").unwrap();
        let b_pos = out.find("b.rs").unwrap();
        assert!(a_pos < b_pos);
    }

    use crate::graph::model::{FileRef, GitStatus, ModuleNode};
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;

    fn file(path: &str) -> FileRef {
        FileRef {
            path: PathBuf::from(path),
            base_blob: Some("b".to_string()),
            head_blob: Some("h".to_string()),
        }
    }

    fn node(id: &str, files: Vec<FileRef>) -> ModuleNode {
        ModuleNode {
            id: NodeId::from(id),
            display_name: id.to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Modified,
            files,
        }
    }

    fn graph_with(nodes: Vec<ModuleNode>) -> ProjectGraph {
        let mut map = StdHashMap::new();
        let mut roots = Vec::new();
        for n in nodes {
            roots.push(n.id.clone());
            map.insert(n.id.clone(), n);
        }
        ProjectGraph {
            nodes: map,
            roots,
            edges: vec![],
        }
    }

    #[test]
    fn map_comments_attaches_by_node_field() {
        let graph = graph_with(vec![node("a", vec![file("a.rs")])]);
        let mut c = comment("unrelated.rs", 1, 1);
        c.node = Some("a".to_string());
        let mapped = map_comments(&graph, &[c]);
        assert_eq!(mapped.get(&NodeId::from("a")).unwrap().len(), 1);
    }

    #[test]
    fn map_comments_skips_unknown_node_id_silently() {
        let graph = graph_with(vec![node("a", vec![file("a.rs")])]);
        let mut c = comment("a.rs", 1, 1);
        c.node = Some("ghost".to_string());
        let mapped = map_comments(&graph, &[c]);
        // The unknown node id contributes nothing, but the path still
        // matches "a" independently.
        assert_eq!(mapped.get(&NodeId::from("a")).unwrap().len(), 1);
        assert!(!mapped.contains_key(&NodeId::from("ghost")));
    }

    #[test]
    fn map_comments_attaches_by_path_to_every_matching_node() {
        let graph = graph_with(vec![
            node("a", vec![file("shared.rs")]),
            node("b", vec![file("shared.rs")]),
            node("c", vec![file("other.rs")]),
        ]);
        let c = comment("shared.rs", 1, 1);
        let mapped = map_comments(&graph, &[c]);
        assert!(mapped.contains_key(&NodeId::from("a")));
        assert!(mapped.contains_key(&NodeId::from("b")));
        assert!(!mapped.contains_key(&NodeId::from("c")));
    }

    #[test]
    fn map_comments_does_not_double_attach_when_node_and_path_match_the_same_node() {
        let graph = graph_with(vec![node("a", vec![file("a.rs")])]);
        let mut c = comment("a.rs", 1, 1);
        c.node = Some("a".to_string());
        let mapped = map_comments(&graph, &[c]);
        assert_eq!(mapped.get(&NodeId::from("a")).unwrap().len(), 1);
    }

    #[test]
    fn map_comments_ignores_comment_matching_nothing() {
        let graph = graph_with(vec![node("a", vec![file("a.rs")])]);
        let c = comment("nope.rs", 1, 1);
        let mapped = map_comments(&graph, &[c]);
        assert!(mapped.is_empty());
    }
}
