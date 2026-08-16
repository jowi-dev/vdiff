//! Render a [`ProjectGraph`] as `--dump json`/`--dump text`.

use crate::cli::DumpFormat;
use crate::graph::model::{GitStatus, NodeId, ProjectGraph};

/// Render `graph` per `format`.
pub fn render(graph: &ProjectGraph, format: DumpFormat) -> String {
    match format {
        DumpFormat::Json => serde_json::to_string_pretty(graph).expect("ProjectGraph serializes"),
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
        let output = render(&fixture(), DumpFormat::Text);
        assert_eq!(output, "M app\n  A alpha\n  D zeta");
    }

    #[test]
    fn json_round_trips_through_project_graph() {
        let graph = fixture();
        let output = render(&graph, DumpFormat::Json);
        let parsed: ProjectGraph = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed, graph);
    }
}
