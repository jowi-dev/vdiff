//! Classifying and hiding Elixir/Rust test modules, and matching a changed
//! test module back to the node it tests. Half the nodes in a typical
//! Elixir change set are `*_test.exs` modules -- hiding them by default
//! (see [`hide_test_modules`], wired up behind the `t` key in
//! [`crate::core::app::Msg::ToggleTests`]) is what turns the graph back into
//! a "call stack story" instead of a wall of test noise.

use std::collections::HashSet;
use std::path::{Component, Path};

use crate::graph::filter::prune;
use crate::graph::model::{GitStatus, ModuleNode, NodeId, ProjectGraph};

/// True if `node` looks like a test module: any backing file sits under a
/// `test`/`tests` path component (an exact component match -- `latest/`
/// must not trip this), or its filename ends `_test.exs`/`_test.ex`, or its
/// display name ends in `Test` (the Elixir `FooTest` convention). A node
/// named `Testimony` does not match the last rule: `Testimony` ends in
/// `mony`, not the literal suffix `Test`.
pub fn is_test_module(node: &ModuleNode) -> bool {
    if node.display_name.ends_with("Test") {
        return true;
    }
    node.files.iter().any(|f| file_looks_like_test(&f.path))
}

/// Whether `path` sits under a `test`/`tests` directory component, or its
/// filename is an Elixir test file (`*_test.exs`/`*_test.ex`).
fn file_looks_like_test(path: &Path) -> bool {
    let under_test_dir = path.components().any(|component| {
        matches!(component, Component::Normal(name) if name == "test" || name == "tests")
    });
    if under_test_dir {
        return true;
    }
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.ends_with("_test.exs") || name.ends_with("_test.ex"),
        None => false,
    }
}

/// Prune every test module (see [`is_test_module`]) out of `graph`, fixing
/// up edges/children/roots the same way [`crate::graph::filter::prune`]
/// does. Ancestors of surviving nodes are untouched -- test modules are
/// leaves in practice, so removing them never orphans a kept node's parent
/// chain. Returns the pruned graph and how many test modules were hidden,
/// so a caller building a legend hint doesn't have to re-scan.
pub fn hide_test_modules(graph: &ProjectGraph) -> (ProjectGraph, usize) {
    let test_ids: HashSet<NodeId> = graph
        .nodes
        .values()
        .filter(|node| is_test_module(node))
        .map(|node| node.id.clone())
        .collect();

    if test_ids.is_empty() {
        return (graph.clone(), 0);
    }

    let keep: HashSet<NodeId> = graph
        .nodes
        .keys()
        .filter(|id| !test_ids.contains(*id))
        .cloned()
        .collect();

    (prune(graph, &keep), test_ids.len())
}

/// The target module a test node tests, if any: `test_node.display_name`
/// with its trailing `Test` stripped, matched against a same-root sibling
/// node's `display_name` (fully-qualified ids like
/// `elixir:App.Leads.LeadTest` and `elixir:App.Leads.Lead` share the same
/// top-level root, but `display_name` only ever carries the last segment --
/// see [`ModuleNode::display_name`] -- so comparing display names alone
/// would over-match across unrelated roots).
fn tested_node_id(graph: &ProjectGraph, test_node: &ModuleNode) -> Option<NodeId> {
    let target_name = test_node.display_name.strip_suffix("Test")?;
    if target_name.is_empty() {
        return None;
    }
    let test_root = graph.top_level_root(&test_node.id);
    graph
        .nodes
        .values()
        .find(|candidate| {
            candidate.display_name == target_name
                && !is_test_module(candidate)
                && graph.top_level_root(&candidate.id) == test_root
        })
        .map(|candidate| candidate.id.clone())
}

/// Every non-test node id that has a matching test module (see
/// [`tested_node_id`]) whose [`GitStatus`] isn't [`GitStatus::Unchanged`] --
/// i.e. nodes worth flagging with the "tested" badge in the graph view.
/// Computed from the full graph regardless of [`hide_test_modules`], so the
/// badge is correct whether or not test nodes are currently hidden.
pub fn nodes_with_changed_tests(graph: &ProjectGraph) -> HashSet<NodeId> {
    graph
        .nodes
        .values()
        .filter(|node| is_test_module(node) && node.status != GitStatus::Unchanged)
        .filter_map(|test_node| tested_node_id(graph, test_node))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepEdge, DepKind, FileRef};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn file(path: &str) -> FileRef {
        FileRef {
            path: PathBuf::from(path),
            base_blob: Some("b".to_string()),
            head_blob: Some("h".to_string()),
        }
    }

    fn node(id: &str, name: &str, status: GitStatus, files: Vec<FileRef>) -> ModuleNode {
        node_with_parent(id, name, status, files, None)
    }

    fn node_with_parent(
        id: &str,
        name: &str,
        status: GitStatus,
        files: Vec<FileRef>,
        parent: Option<&str>,
    ) -> ModuleNode {
        ModuleNode {
            id: NodeId::from(id),
            display_name: name.to_string(),
            parent: parent.map(NodeId::from),
            children: vec![],
            status,
            files,
        }
    }

    #[test]
    fn matches_display_name_ending_in_test() {
        let n = node(
            "elixir:App.LeadTest",
            "LeadTest",
            GitStatus::Unchanged,
            vec![file("test/app/lead_test.exs")],
        );
        assert!(is_test_module(&n));
    }

    #[test]
    fn does_not_match_name_that_merely_contains_test_as_a_substring() {
        let n = node(
            "elixir:App.Testimony",
            "Testimony",
            GitStatus::Unchanged,
            vec![file("lib/app/testimony.ex")],
        );
        assert!(!is_test_module(&n));
    }

    #[test]
    fn matches_file_under_a_test_directory_component() {
        let n = node(
            "elixir:App.Fixtures",
            "Fixtures",
            GitStatus::Unchanged,
            vec![file("test/support/fixtures.ex")],
        );
        assert!(is_test_module(&n));
    }

    #[test]
    fn matches_file_under_a_tests_directory_component() {
        let n = node(
            "rust:helpers",
            "helpers",
            GitStatus::Unchanged,
            vec![file("tests/helpers.rs")],
        );
        assert!(is_test_module(&n));
    }

    #[test]
    fn does_not_match_a_latest_directory_component() {
        // "latest" contains "test" as a substring but is not the path
        // component "test" -- must not match.
        let n = node(
            "elixir:App.Latest",
            "Latest",
            GitStatus::Unchanged,
            vec![file("lib/app/latest/report.ex")],
        );
        assert!(!is_test_module(&n));
    }

    #[test]
    fn matches_filename_suffix_outside_a_test_directory() {
        let n = node(
            "elixir:App.LeadTest",
            "LeadTest2",
            GitStatus::Unchanged,
            vec![file("lib/app/lead_test.exs")],
        );
        assert!(is_test_module(&n));
    }

    #[test]
    fn does_not_match_an_ordinary_lib_file() {
        let n = node(
            "elixir:App.Lead",
            "Lead",
            GitStatus::Unchanged,
            vec![file("lib/app/lead.ex")],
        );
        assert!(!is_test_module(&n));
    }

    fn graph_with(nodes: Vec<ModuleNode>, edges: Vec<DepEdge>) -> ProjectGraph {
        let roots = nodes.iter().map(|n| n.id.clone()).collect();
        ProjectGraph {
            nodes: nodes.into_iter().map(|n| (n.id.clone(), n)).collect(),
            roots,
            edges,
        }
    }

    #[test]
    fn hide_test_modules_removes_test_nodes_and_their_edges() {
        let lead = node(
            "elixir:App.Lead",
            "Lead",
            GitStatus::Modified,
            vec![file("lib/app/lead.ex")],
        );
        let lead_test = node(
            "elixir:App.LeadTest",
            "LeadTest",
            GitStatus::Modified,
            vec![file("test/app/lead_test.exs")],
        );
        let g = graph_with(
            vec![lead.clone(), lead_test.clone()],
            vec![DepEdge {
                from: lead_test.id.clone(),
                to: lead.id.clone(),
                kind: DepKind::XrefCall,
            }],
        );

        let (hidden_graph, count) = hide_test_modules(&g);

        assert_eq!(count, 1);
        assert!(hidden_graph.node(&lead.id).is_some());
        assert!(hidden_graph.node(&lead_test.id).is_none());
        assert!(hidden_graph.edges.is_empty());
        assert_eq!(hidden_graph.roots, vec![lead.id]);
    }

    #[test]
    fn hide_test_modules_is_a_noop_when_there_are_no_test_nodes() {
        let lead = node(
            "elixir:App.Lead",
            "Lead",
            GitStatus::Modified,
            vec![file("lib/app/lead.ex")],
        );
        let g = graph_with(vec![lead], vec![]);

        let (hidden_graph, count) = hide_test_modules(&g);

        assert_eq!(count, 0);
        assert_eq!(hidden_graph, g);
    }

    #[test]
    fn nodes_with_changed_tests_matches_short_name_and_requires_status_change() {
        // `lead`/`lead_test_changed` share a parent (so they share a
        // top-level root) distinct from `other`/`other_test_unchanged`'s --
        // `top_level_root` walks `parent`, not the id string, so this is
        // what actually makes them "same root" for matching purposes.
        let lead = node_with_parent(
            "elixir:App.Leads.Lead",
            "Lead",
            GitStatus::Unchanged,
            vec![file("lib/app/leads/lead.ex")],
            Some("elixir:App.Leads"),
        );
        let lead_test_changed = node_with_parent(
            "elixir:App.Leads.LeadTest",
            "LeadTest",
            GitStatus::Modified,
            vec![file("test/app/leads/lead_test.exs")],
            Some("elixir:App.Leads"),
        );
        let other = node_with_parent(
            "elixir:App.Billing",
            "Billing",
            GitStatus::Unchanged,
            vec![file("lib/app/billing.ex")],
            Some("elixir:App.BillingNs"),
        );
        let other_test_unchanged = node_with_parent(
            "elixir:App.BillingTest",
            "BillingTest",
            GitStatus::Unchanged,
            vec![file("test/app/billing_test.exs")],
            Some("elixir:App.BillingNs"),
        );
        let g = graph_with(
            vec![lead, lead_test_changed, other, other_test_unchanged],
            vec![],
        );

        let flagged = nodes_with_changed_tests(&g);

        assert_eq!(
            flagged,
            HashSet::from([NodeId::from("elixir:App.Leads.Lead")])
        );
    }

    #[test]
    fn nodes_with_changed_tests_does_not_cross_match_across_roots() {
        let mut nodes_map: HashMap<NodeId, ModuleNode> = HashMap::new();
        let a_lead = node(
            "elixir:A.Lead",
            "Lead",
            GitStatus::Unchanged,
            vec![file("apps/a/lib/lead.ex")],
        );
        let b_lead_test = node(
            "elixir:B.LeadTest",
            "LeadTest",
            GitStatus::Modified,
            vec![file("apps/b/test/lead_test.exs")],
        );
        nodes_map.insert(a_lead.id.clone(), a_lead.clone());
        nodes_map.insert(b_lead_test.id.clone(), b_lead_test.clone());
        let g = ProjectGraph {
            roots: vec![a_lead.id.clone(), b_lead_test.id.clone()],
            nodes: nodes_map,
            edges: vec![],
        };

        // `a_lead` and `b_lead_test` are each their own top-level root (no
        // parent), so they don't share a root and must not cross-match even
        // though the names line up.
        assert!(nodes_with_changed_tests(&g).is_empty());
    }
}
