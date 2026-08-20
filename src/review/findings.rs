//! Pure data model for AI review findings: the [`Finding`] struct produced
//! by a review agent run over `vdiff --dump json --include-diffs`'s output,
//! its serde round-trip and load-time validation, and the pure helpers that
//! map findings onto graph nodes and summarize them for painting. Zero IO --
//! reading `findings.json` off disk lives in `main.rs`'s own startup
//! module, which calls [`parse_findings`] then [`map_findings`]. See
//! `docs/findings-schema.md` for the wire contract this struct mirrors.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::graph::model::{NodeId, ProjectGraph};

/// A finding's severity, driving both its badge color (see
/// [`crate::ui::theme::severity_color`]) and the ranking [`worst_severity`]
/// uses to pick one color for a node with more than one finding.
/// Lowercase in JSON (`"low"`/`"medium"`/`"high"`) -- matches the rest of
/// this schema's field-naming convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

/// One review finding: either anchored to a graph node directly (`node_id`,
/// set by an agent that already knows vdiff's node ids from the `--dump
/// json` payload it read) or to a source file (`path`, repo-relative,
/// matched against [`crate::graph::model::FileRef::path`] the same way
/// issue #14's comment-mapping does), optionally narrowed to one `line`
/// within that file. See [`parse_findings`] for the "at least one of
/// `node_id`/`path`" validation this type doesn't enforce on its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// The graph node this finding is about, if the agent already knows
    /// vdiff's node id (e.g. copied straight from the `--dump json` payload
    /// it read). Takes priority over `path` in [`map_findings`] when both
    /// are set.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub node_id: Option<String>,
    /// Repo-relative path this finding is about, used to resolve a node
    /// when `node_id` is absent (or unknown -- see [`map_findings`]'s doc).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<PathBuf>,
    /// 1-based line within `path` the finding is about, if it's line-level
    /// rather than file/node-level.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub line: Option<u32>,
    pub severity: Severity,
    /// A short, one-line description -- what's shown in the graph badge's
    /// tooltip-equivalent (the focus overlay) and the file pane's inline
    /// annotation.
    pub summary: String,
    /// Optional longer explanation. Not rendered anywhere yet (the overlay
    /// and file pane both stay to `summary` per the issue's "keep it
    /// simple" guidance); carried through so a future detail view has
    /// somewhere to read it from without a schema change.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
}

/// Parse `contents` (the raw text of a `findings.json` file) into a list of
/// [`Finding`]s, validating that every entry has at least one of
/// `node_id`/`path` set -- a finding anchored to neither is meaningless (it
/// can never be attached to a node), and silently dropping it would hide a
/// review agent's mistake rather than surface it. Errors name the failing
/// entry's index (0-based) so a human -- or the agent itself, if the error
/// is fed back to it -- can find the bad entry without re-deriving the
/// index by hand.
pub fn parse_findings(contents: &str) -> Result<Vec<Finding>, String> {
    let findings: Vec<Finding> =
        serde_json::from_str(contents).map_err(|err| format!("invalid findings JSON: {err}"))?;
    for (index, finding) in findings.iter().enumerate() {
        if finding.node_id.is_none() && finding.path.is_none() {
            return Err(format!(
                "finding at index {index} has neither node_id nor path set"
            ));
        }
    }
    Ok(findings)
}

/// [`map_findings`]'s result: every finding successfully attached to at
/// least one node, plus the indices (into the original `findings` slice) of
/// any finding that couldn't be attached to anything -- an unknown
/// `node_id`, or a `path` that matches no node's files in the current
/// graph. Neither case is fatal (see [`map_findings`]'s doc); the caller
/// decides what, if anything, to warn about `unmatched`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MappedFindings {
    pub by_node: HashMap<NodeId, Vec<Finding>>,
    pub unmatched: Vec<usize>,
}

/// Attach every finding in `findings` to the node(s) it's about: `node_id`
/// wins when set (looked up directly against `graph`), falling back to
/// `path` matched against every node's [`crate::graph::model::FileRef::path`]
/// -- one file can back more than one node (an Elixir file with several
/// `defmodule`s, see [`crate::graph::model::FileRef`]'s doc), so a path
/// match can attach to more than one node, same spirit as issue #14's
/// comment-mapping. An unknown `node_id`, or a `path` matching nothing in
/// `graph`, isn't an error: findings are commonly run over the full `--dump
/// json --include-diffs` payload, which can name paths/nodes the *currently
/// displayed* (possibly `focus_on_changes`-filtered) graph doesn't include
/// -- that's an expected mismatch, not an agent-contract violation, so it's
/// reported back via [`MappedFindings::unmatched`] for the caller to warn
/// about rather than failing the whole load.
pub fn map_findings(graph: &ProjectGraph, findings: &[Finding]) -> MappedFindings {
    let mut result = MappedFindings::default();
    for (index, finding) in findings.iter().enumerate() {
        let targets = resolve_targets(graph, finding);
        if targets.is_empty() {
            result.unmatched.push(index);
            continue;
        }
        for target in targets {
            result
                .by_node
                .entry(target)
                .or_default()
                .push(finding.clone());
        }
    }
    result
}

/// The node id(s) `finding` resolves to in `graph`: `node_id` directly
/// (exactly one hit, or none if unknown), else every node whose `files`
/// contains `path`.
fn resolve_targets(graph: &ProjectGraph, finding: &Finding) -> Vec<NodeId> {
    if let Some(node_id) = &finding.node_id {
        let id = NodeId::from(node_id.clone());
        return if graph.node(&id).is_some() {
            vec![id]
        } else {
            Vec::new()
        };
    }
    let Some(path) = &finding.path else {
        return Vec::new();
    };
    graph
        .nodes
        .values()
        .filter(|node| node.files.iter().any(|f| &f.path == path))
        .map(|node| node.id.clone())
        .collect()
}

/// The highest [`Severity`] present in `findings`, or `None` if empty --
/// what the graph badge (see [`crate::ui::graph_view`]) colors itself by
/// when a node has more than one finding.
pub fn worst_severity(findings: &[Finding]) -> Option<Severity> {
    findings.iter().map(|f| f.severity).max()
}

/// `(count, worst severity)` for `findings`, or `None` for an empty slice --
/// exactly the two pieces of information the graph badge paints. A thin
/// wrapper over [`worst_severity`] plus a length check, kept separate so
/// callers that only need the severity (the focus overlay's per-line color)
/// don't have to unpack a tuple they'd ignore half of.
pub fn badge(findings: &[Finding]) -> Option<(usize, Severity)> {
    worst_severity(findings).map(|severity| (findings.len(), severity))
}

/// One findings line for the focus overlay/file pane: `"[high] summary"`.
/// Pure text assembly, kept separate from any painting so it's
/// unit-testable without egui.
pub fn format_finding_line(finding: &Finding) -> String {
    format!("[{}] {}", severity_label(finding.severity), finding.summary)
}

/// Lowercase severity label matching the JSON wire format, for
/// [`format_finding_line`].
fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
    }
}

/// Every finding in `findings` with `line == Some(target_line)`
/// (1-based, matching [`Finding::line`]'s own convention) -- what the
/// built-in file pane's gutter marker looks up per rendered row.
pub fn findings_at_line(findings: &[Finding], target_line: u32) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|f| f.line == Some(target_line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{FileRef, GitStatus, ModuleNode};
    use std::collections::HashMap as StdHashMap;

    fn finding(node_id: Option<&str>, path: Option<&str>, severity: Severity) -> Finding {
        Finding {
            node_id: node_id.map(str::to_string),
            path: path.map(PathBuf::from),
            line: None,
            severity,
            summary: "some summary".to_string(),
            detail: None,
        }
    }

    #[test]
    fn serde_round_trip_preserves_fields() {
        let mut f = finding(Some("rust:crate"), None, Severity::High);
        f.line = Some(12);
        f.detail = Some("longer text".to_string());
        let json = serde_json::to_string(&f).expect("serialize");
        let back: Finding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(f, back);
    }

    #[test]
    fn severity_serializes_lowercase() {
        let f = finding(Some("n"), None, Severity::High);
        let json = serde_json::to_string(&f).expect("serialize");
        assert!(json.contains("\"high\""));
    }

    #[test]
    fn parse_findings_rejects_entry_with_neither_node_id_nor_path() {
        let json = r#"[{"severity":"low","summary":"oops"}]"#;
        let err = parse_findings(json).unwrap_err();
        assert!(
            err.contains("index 0"),
            "error should name the index: {err}"
        );
    }

    #[test]
    fn parse_findings_names_the_failing_index_past_the_first_entry() {
        let json = r#"[
            {"node_id":"a","severity":"low","summary":"fine"},
            {"severity":"low","summary":"oops"}
        ]"#;
        let err = parse_findings(json).unwrap_err();
        assert!(err.contains("index 1"), "error should name index 1: {err}");
    }

    #[test]
    fn parse_findings_accepts_node_id_only() {
        let json = r#"[{"node_id":"a","severity":"low","summary":"fine"}]"#;
        let findings = parse_findings(json).expect("should parse");
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn parse_findings_accepts_path_only() {
        let json = r#"[{"path":"src/lib.rs","severity":"low","summary":"fine"}]"#;
        let findings = parse_findings(json).expect("should parse");
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn parse_findings_rejects_invalid_json() {
        assert!(parse_findings("not json").is_err());
    }

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
    fn map_findings_attaches_by_node_id() {
        let graph = graph_with(vec![node("a", vec![file("a.rs")])]);
        let findings = vec![finding(Some("a"), None, Severity::Medium)];
        let mapped = map_findings(&graph, &findings);
        assert_eq!(mapped.by_node.get(&NodeId::from("a")).unwrap().len(), 1);
        assert!(mapped.unmatched.is_empty());
    }

    #[test]
    fn map_findings_reports_unknown_node_id_as_unmatched() {
        let graph = graph_with(vec![node("a", vec![file("a.rs")])]);
        let findings = vec![finding(Some("ghost"), None, Severity::Medium)];
        let mapped = map_findings(&graph, &findings);
        assert!(mapped.by_node.is_empty());
        assert_eq!(mapped.unmatched, vec![0]);
    }

    #[test]
    fn map_findings_attaches_by_path_to_every_matching_node() {
        // Two nodes share a backing file (the one-file-many-modules case).
        let graph = graph_with(vec![
            node("a", vec![file("shared.ex")]),
            node("b", vec![file("shared.ex")]),
            node("c", vec![file("other.ex")]),
        ]);
        let findings = vec![finding(None, Some("shared.ex"), Severity::Low)];
        let mapped = map_findings(&graph, &findings);
        assert!(mapped.by_node.contains_key(&NodeId::from("a")));
        assert!(mapped.by_node.contains_key(&NodeId::from("b")));
        assert!(!mapped.by_node.contains_key(&NodeId::from("c")));
        assert!(mapped.unmatched.is_empty());
    }

    #[test]
    fn map_findings_reports_unmatched_path_as_unmatched() {
        let graph = graph_with(vec![node("a", vec![file("a.rs")])]);
        let findings = vec![finding(None, Some("nope.rs"), Severity::Low)];
        let mapped = map_findings(&graph, &findings);
        assert!(mapped.by_node.is_empty());
        assert_eq!(mapped.unmatched, vec![0]);
    }

    #[test]
    fn map_findings_node_id_wins_over_path_when_both_set() {
        let graph = graph_with(vec![
            node("a", vec![file("a.rs")]),
            node("b", vec![file("b.rs")]),
        ]);
        let mut f = finding(Some("a"), Some("b.rs"), Severity::Low);
        f.node_id = Some("a".to_string());
        let mapped = map_findings(&graph, &[f]);
        assert!(mapped.by_node.contains_key(&NodeId::from("a")));
        assert!(!mapped.by_node.contains_key(&NodeId::from("b")));
    }

    #[test]
    fn worst_severity_picks_the_highest_present() {
        let findings = vec![
            finding(Some("a"), None, Severity::Low),
            finding(Some("a"), None, Severity::High),
            finding(Some("a"), None, Severity::Medium),
        ];
        assert_eq!(worst_severity(&findings), Some(Severity::High));
    }

    #[test]
    fn worst_severity_none_for_empty() {
        assert_eq!(worst_severity(&[]), None);
    }

    #[test]
    fn badge_reports_count_and_worst_severity() {
        let findings = vec![
            finding(Some("a"), None, Severity::Low),
            finding(Some("a"), None, Severity::High),
        ];
        assert_eq!(badge(&findings), Some((2, Severity::High)));
    }

    #[test]
    fn badge_none_for_empty() {
        assert_eq!(badge(&[]), None);
    }

    #[test]
    fn format_finding_line_includes_severity_and_summary() {
        let mut f = finding(Some("a"), None, Severity::High);
        f.summary = "Possible null deref".to_string();
        assert_eq!(format_finding_line(&f), "[high] Possible null deref");
    }

    #[test]
    fn findings_at_line_filters_by_exact_line() {
        let mut f1 = finding(Some("a"), None, Severity::Low);
        f1.line = Some(10);
        let mut f2 = finding(Some("a"), None, Severity::High);
        f2.line = Some(20);
        let findings = vec![f1.clone(), f2.clone()];
        assert_eq!(findings_at_line(&findings, 10), vec![&f1]);
        assert_eq!(findings_at_line(&findings, 20), vec![&f2]);
        assert!(findings_at_line(&findings, 30).is_empty());
    }

    #[test]
    fn findings_at_line_excludes_line_less_findings() {
        let f = finding(Some("a"), None, Severity::Low);
        assert!(findings_at_line(&[f], 1).is_empty());
    }
}
