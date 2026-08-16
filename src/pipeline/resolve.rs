//! Resolve [`DepRef`]s (dependency references collected by
//! [`crate::pipeline::extract`]) into [`DepEdge`]s over the whole project's
//! node table.
//!
//! Rust: `crate::`/`self::`/`super::` prefixes are substituted for the
//! referencing node's own crate name / own id / parent id; anything else
//! (including a bare workspace-crate-qualified path like
//! `other_crate::Thing`) is tried as-is. Either way, the resulting
//! candidate is matched against the node table by progressively trimming
//! trailing segments -- this is what lets a `use` path that names an item
//! inside a module (`crate::pipeline::repo::GitRepo`) resolve to that
//! module's node (`myapp::pipeline::repo`) rather than needing an exact
//! hit, and what makes genuinely external deps (`serde::Serialize`) drop
//! silently once trimming runs out of segments with no match.
//!
//! Elixir: no `crate::`/`self::`/`super::` syntax exists, so a [`DepRef`]'s
//! name -- already the literal text of the alias/import/use/require target
//! -- is matched the same progressive way. Per the v1 scope, only the
//! directive itself becomes an edge; a subsequent bare reference the alias
//! brought into scope (e.g. using `Repo` after `alias MyApp.Repo`) is not
//! chased.
//!
//! Known limitation: `super::` is only substituted once per path, against
//! the referencing node's immediate parent -- a chained `super::super::x`
//! is not specially handled (the leftover `super::` segment is treated as
//! a literal path component, which will usually fail to resolve or,
//! rarely, match the wrong ancestor via progressive trimming).

use std::collections::HashMap;

use crate::graph::model::{DepEdge, ModuleNode, NodeId};
use crate::pipeline::extract::DepRef;

/// A node's own dependency references, plus its Rust crate name if it is a
/// Rust node. `None` for Elixir/Other nodes, which have no
/// `crate::`/`self::`/`super::` syntax to substitute.
pub struct NodeContext {
    pub id: NodeId,
    pub dep_refs: Vec<DepRef>,
    pub rust_crate_name: Option<String>,
}

/// Resolve every context's `dep_refs` into edges against `nodes`.
/// Unresolvable references are dropped silently.
pub fn resolve_edges(
    nodes: &HashMap<NodeId, ModuleNode>,
    contexts: &[NodeContext],
) -> Vec<DepEdge> {
    let mut edges = Vec::new();
    for ctx in contexts {
        for dep in &ctx.dep_refs {
            if let Some(target) = resolve_one(nodes, ctx, dep) {
                edges.push(DepEdge {
                    from: ctx.id.clone(),
                    to: target,
                    kind: dep.kind,
                });
            }
        }
    }
    edges
}

fn resolve_one(
    nodes: &HashMap<NodeId, ModuleNode>,
    ctx: &NodeContext,
    dep: &DepRef,
) -> Option<NodeId> {
    let sep = if ctx.rust_crate_name.is_some() {
        "::"
    } else {
        "."
    };
    let candidate = substitute_prefix(nodes, ctx, &dep.name, sep);
    progressive_match(nodes, &candidate, sep)
}

/// Substitute a Rust `crate::`/`self::`/`super::` prefix (or the bare
/// `crate`/`self`/`super` forms) for the concrete path it names. A no-op
/// for Elixir/Other contexts (`rust_crate_name` is `None`) and for any Rust
/// path that isn't so prefixed.
fn substitute_prefix(
    nodes: &HashMap<NodeId, ModuleNode>,
    ctx: &NodeContext,
    raw: &str,
    sep: &str,
) -> String {
    let Some(crate_name) = &ctx.rust_crate_name else {
        return raw.to_string();
    };
    let home = ctx.id.to_string();

    if raw == "crate" {
        return crate_name.clone();
    }
    if let Some(rest) = raw.strip_prefix("crate::") {
        return join(crate_name, rest, sep);
    }
    if raw == "self" {
        return home;
    }
    if let Some(rest) = raw.strip_prefix("self::") {
        return join(&home, rest, sep);
    }
    if raw == "super" || raw.starts_with("super::") {
        let parent = nodes
            .get(&ctx.id)
            .and_then(|n| n.parent.clone())
            .map(|p| p.to_string())
            .unwrap_or_else(|| crate_name.clone());
        return match raw.strip_prefix("super::") {
            Some(rest) => join(&parent, rest, sep),
            None => parent,
        };
    }
    raw.to_string()
}

fn join(prefix: &str, rest: &str, sep: &str) -> String {
    match (prefix.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}{sep}{rest}"),
    }
}

/// Match `candidate` against `nodes`, trimming trailing `sep`-separated
/// segments until a node is found or the candidate is exhausted.
fn progressive_match(
    nodes: &HashMap<NodeId, ModuleNode>,
    candidate: &str,
    sep: &str,
) -> Option<NodeId> {
    let mut current = candidate;
    loop {
        if current.is_empty() {
            return None;
        }
        let id = NodeId::from(current.to_string());
        if nodes.contains_key(&id) {
            return Some(id);
        }
        current = current.rsplit_once(sep)?.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepKind, GitStatus};

    fn node(id: &str, parent: Option<&str>) -> ModuleNode {
        ModuleNode {
            id: NodeId::from(id),
            display_name: id.rsplit("::").next().unwrap_or(id).to_string(),
            parent: parent.map(NodeId::from),
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![],
        }
    }

    fn nodes(pairs: &[(&str, Option<&str>)]) -> HashMap<NodeId, ModuleNode> {
        pairs
            .iter()
            .map(|(id, parent)| (NodeId::from(*id), node(id, *parent)))
            .collect()
    }

    fn dep(name: &str) -> DepRef {
        DepRef {
            name: name.to_string(),
            kind: DepKind::Use,
        }
    }

    #[test]
    fn resolves_crate_prefixed_path_to_item_owning_module() {
        let nodes = nodes(&[
            ("myapp", None),
            ("myapp::pipeline", Some("myapp")),
            ("myapp::pipeline::repo", Some("myapp::pipeline")),
        ]);
        let ctx = NodeContext {
            id: NodeId::from("myapp"),
            dep_refs: vec![dep("crate::pipeline::repo::GitRepo")],
            rust_crate_name: Some("myapp".to_string()),
        };
        let edges = resolve_edges(&nodes, &[ctx]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to, NodeId::from("myapp::pipeline::repo"));
    }

    #[test]
    fn resolves_bare_workspace_crate_path() {
        let nodes = nodes(&[
            ("other_crate", None),
            ("other_crate::thing", Some("other_crate")),
        ]);
        let ctx = NodeContext {
            id: NodeId::from("myapp"),
            dep_refs: vec![dep("other_crate::thing::Item")],
            rust_crate_name: Some("myapp".to_string()),
        };
        let edges = resolve_edges(&nodes, &[ctx]);
        assert_eq!(edges[0].to, NodeId::from("other_crate::thing"));
    }

    #[test]
    fn drops_unresolvable_external_dep() {
        let nodes = nodes(&[("myapp", None)]);
        let ctx = NodeContext {
            id: NodeId::from("myapp"),
            dep_refs: vec![dep("serde::Serialize")],
            rust_crate_name: Some("myapp".to_string()),
        };
        assert!(resolve_edges(&nodes, &[ctx]).is_empty());
    }

    #[test]
    fn resolves_single_super_relative_to_parent() {
        let nodes = nodes(&[
            ("myapp", None),
            ("myapp::foo", Some("myapp")),
            ("myapp::foo::bar", Some("myapp::foo")),
            ("myapp::foo::sibling", Some("myapp::foo")),
        ]);
        let ctx = NodeContext {
            id: NodeId::from("myapp::foo::bar"),
            dep_refs: vec![dep("super::sibling")],
            rust_crate_name: Some("myapp".to_string()),
        };
        let edges = resolve_edges(&nodes, &[ctx]);
        assert_eq!(edges[0].to, NodeId::from("myapp::foo::sibling"));
    }

    #[test]
    fn elixir_exact_dotted_match_with_no_prefix_syntax() {
        let nodes = nodes(&[("MyApp", None), ("MyApp.Repo", Some("MyApp"))]);
        let ctx = NodeContext {
            id: NodeId::from("MyApp"),
            dep_refs: vec![dep("MyApp.Repo")],
            rust_crate_name: None,
        };
        let edges = resolve_edges(&nodes, &[ctx]);
        assert_eq!(edges[0].to, NodeId::from("MyApp.Repo"));
    }
}
