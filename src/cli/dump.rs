//! Render a [`ProjectGraph`] as `--dump json`/`--dump text`.
//!
//! `--dump json`'s top-level shape is `{ "graph": ProjectGraph }`; with
//! `--include-diffs`, a sibling `"diffs"` key appears:
//! `{ "graph": ProjectGraph, "diffs": { "<node id>": [FileDiffEntry, ...] } }`.
//! `diffs` is a map from a node's stringified [`NodeId`] to that node's
//! [`FileDiffEntry`] list (see [`crate::pipeline::file_diff::diffs_for_graph`]
//! for how it's computed); nodes with no changes are simply absent from the
//! map, not present with an empty list. Without `--include-diffs`, the
//! `"diffs"` key is entirely absent (not `null`) -- this is the stable
//! machine contract for the AI-review payload, so keep both shapes (with
//! and without diffs) in mind before changing field names here or on
//! [`crate::diffing::hunks::FileDiff`]/[`crate::diffing::hunks::DiffHunk`]/
//! [`crate::diffing::hunks::LinePair`].
//!
//! `--dump text` always stays graph-only; it never renders diff content.

use std::collections::HashMap;

use serde::Serialize;

use crate::cli::DumpFormat;
use crate::diffing::hunks::FileDiffEntry;
use crate::graph::model::{GitStatus, NodeId, ProjectGraph};

/// The `--dump json` top-level envelope. See the module docs for the exact
/// shape with and without `--include-diffs`.
#[derive(Serialize)]
struct DumpEnvelope<'a> {
    graph: &'a ProjectGraph,
    #[serde(skip_serializing_if = "Option::is_none")]
    diffs: Option<&'a HashMap<String, Vec<FileDiffEntry>>>,
}

/// Render `graph` per `format`. `diffs`, if given, is only used for
/// [`DumpFormat::Json`] -- [`DumpFormat::Text`] ignores it and stays
/// graph-only.
pub fn render(
    graph: &ProjectGraph,
    format: DumpFormat,
    diffs: Option<&HashMap<String, Vec<FileDiffEntry>>>,
) -> String {
    match format {
        DumpFormat::Json => {
            let envelope = DumpEnvelope { graph, diffs };
            serde_json::to_string_pretty(&envelope).expect("DumpEnvelope serializes")
        }
        DumpFormat::Text => render_text(graph),
    }
}

/// An indented tree, one node per line: status letter, a space, then
/// `display_name`. Sibling order is always [`ProjectGraph::sorted_children`]/
/// [`ProjectGraph::sorted_roots`] (name-sorted), so output is stable across
/// runs.
fn render_text(graph: &ProjectGraph) -> String {
    let mut out = String::new();
    for root in graph.sorted_roots() {
        render_node(graph, &root, 0, &mut out);
    }
    let trimmed_len = out.trim_end_matches('\n').len();
    out.truncate(trimmed_len);
    out
}

fn render_node(graph: &ProjectGraph, id: &NodeId, depth: usize, out: &mut String) {
    let Some(node) = graph.node(id) else {
        return;
    };
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push(status_letter(node.status));
    out.push(' ');
    out.push_str(&node.display_name);
    out.push('\n');
    for child in graph.sorted_children(id) {
        render_node(graph, &child, depth + 1, out);
    }
}

fn status_letter(status: GitStatus) -> char {
    match status {
        GitStatus::Unchanged => 'U',
        GitStatus::Added => 'A',
        GitStatus::Modified => 'M',
        GitStatus::Deleted => 'D',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepEdge, DepKind, ModuleNode};
    use std::collections::HashMap;

    /// One root ("app", modified) with two children: "alpha" (added, name-
    /// sorted first) and "zeta" (deleted), plus a dep edge alpha -> zeta
    /// (irrelevant to the text renderer, present to prove JSON round-trips
    /// edges too).
    fn fixture() -> ProjectGraph {
        let root = NodeId::from("app");
        let alpha = NodeId::from("app::alpha");
        let zeta = NodeId::from("app::zeta");

        let mut nodes = HashMap::new();
        nodes.insert(
            root.clone(),
            ModuleNode {
                id: root.clone(),
                display_name: "app".to_string(),
                parent: None,
                children: vec![alpha.clone(), zeta.clone()],
                status: GitStatus::Modified,
                files: vec![],
            },
        );
        nodes.insert(
            alpha.clone(),
            ModuleNode {
                id: alpha.clone(),
                display_name: "alpha".to_string(),
                parent: Some(root.clone()),
                children: vec![],
                status: GitStatus::Added,
                files: vec![],
            },
        );
        nodes.insert(
            zeta.clone(),
            ModuleNode {
                id: zeta.clone(),
                display_name: "zeta".to_string(),
                parent: Some(root.clone()),
                children: vec![],
                status: GitStatus::Deleted,
                files: vec![],
            },
        );

        ProjectGraph {
            nodes,
            roots: vec![root],
            edges: vec![DepEdge {
                from: alpha,
                to: zeta,
                kind: DepKind::Use,
            }],
        }
    }

    #[test]
    fn text_renders_indented_status_letters_in_name_sorted_order() {
        let output = render(&fixture(), DumpFormat::Text, None);
        assert_eq!(output, "M app\n  A alpha\n  D zeta");
    }

    /// Top-level JSON shape without `--include-diffs`: `{ "graph": ... }`,
    /// with no `"diffs"` key at all (not `null`).
    #[test]
    fn json_envelope_without_diffs_has_only_a_graph_key() {
        let graph = fixture();
        let output = render(&graph, DumpFormat::Json, None);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let obj = parsed.as_object().unwrap();
        assert_eq!(obj.keys().collect::<Vec<_>>(), vec!["graph"]);

        let parsed_graph: ProjectGraph = serde_json::from_value(obj["graph"].clone()).unwrap();
        assert_eq!(parsed_graph, graph);
    }

    /// With `--include-diffs`, the envelope gains a `"diffs"` map keyed by
    /// stringified node id.
    #[test]
    fn json_envelope_with_diffs_includes_the_diffs_map() {
        use crate::diffing::hunks::{DiffHunk, FileDiffEntry, LinePair};
        use std::path::PathBuf;

        let graph = fixture();
        let mut diffs = HashMap::new();
        diffs.insert(
            "app::alpha".to_string(),
            vec![FileDiffEntry {
                path: PathBuf::from("src/alpha.rs"),
                hunks: vec![DiffHunk {
                    lines: vec![LinePair::Added { head: 0 }],
                }],
                base_lines: vec![],
                head_lines: vec!["new line".to_string()],
            }],
        );

        let output = render(&graph, DumpFormat::Json, Some(&diffs));
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let obj = parsed.as_object().unwrap();
        assert!(obj.contains_key("diffs"));
        assert!(obj["diffs"].get("app::alpha").is_some());
        assert!(
            obj["diffs"].get("app::zeta").is_none(),
            "node with no scripted diff entry must be absent from the map"
        );
    }
}
