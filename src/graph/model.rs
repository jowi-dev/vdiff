//! Pure graph data model: the nodes/edges vdiff renders, and the exact shape
//! serialized as `--dump json`. Zero dependencies on egui/git2/syn/
//! tree-sitter -- this module only knows about serde and std.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Stable identifier for a [`ModuleNode`], qualified by crate/app so
/// workspace and umbrella-project nodes never collide (e.g.
/// `rust:myapp::foo::bar`, `elixir:MyApp.Accounts.User`), and prefixed with
/// a language namespace (`rust:`, `elixir:`, `file:` for everything else --
/// see [`crate::graph::builder`] for the full convention) so that a
/// single-segment Rust crate and an Elixir module of the same name never
/// collide either. Ids are opaque to consumers; [`ModuleNode::display_name`]
/// carries the human-readable label. Serializes as a bare string
/// (`#[serde(transparent)]`), matching the `--dump json` payload shape.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for NodeId {
    fn from(value: &str) -> Self {
        NodeId(value.to_string())
    }
}

impl From<String> for NodeId {
    fn from(value: String) -> Self {
        NodeId(value)
    }
}

/// A node's git status relative to the diff base, driving its color in the
/// graph view: gray unchanged, green added, yellow modified, red deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitStatus {
    /// No changes between base and head.
    Unchanged,
    /// Present at head, absent at base.
    Added,
    /// Present at both, content differs.
    Modified,
    /// Present at base, absent at head.
    Deleted,
}

/// One file backing a [`ModuleNode`]. File-to-module is one-to-many (a
/// single Elixir file can `defmodule` several modules), so a node's `files`
/// list may repeat across nodes. Blob ids are hex strings rather than
/// `git2::Oid` -- this layer stays git2-free. `base_blob` is `None` for
/// added files, `head_blob` is `None` for deleted files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    /// Path relative to the repository root.
    pub path: PathBuf,
    /// Blob id (hex) at the diff base, or `None` if the file is new.
    pub base_blob: Option<String>,
    /// Blob id (hex) at head, or `None` if the file was deleted.
    pub head_blob: Option<String>,
}

/// One module/file cluster in the project hierarchy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleNode {
    /// This node's stable id.
    pub id: NodeId,
    /// Human-readable name shown in the graph view and used to order
    /// siblings (see [`ProjectGraph::sorted_children`]).
    pub display_name: String,
    /// Parent node id, `None` for roots.
    pub parent: Option<NodeId>,
    /// Child node ids, stored sorted by display name (tie-broken by id) --
    /// see [`ProjectGraph::sorted_children`], which agrees with this order
    /// but is a fresh (re-filtered, re-sorted) copy, not a raw field read.
    pub children: Vec<NodeId>,
    /// This node's git status relative to the diff base.
    pub status: GitStatus,
    /// Files backing this node.
    pub files: Vec<FileRef>,
}

/// The kind of dependency a [`DepEdge`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepKind {
    /// Rust `use`.
    Use,
    /// Elixir `import`.
    Import,
    /// Elixir `alias`.
    Alias,
    /// Elixir `require`.
    Require,
    /// An Elixir fully qualified remote call (`App.Leads.create_lead(...)`)
    /// or struct literal (`%App.Leads.Lead{}`) with no `alias` directive
    /// bringing the target module into scope.
    RemoteCall,
    /// A cross-reference call resolved via `mix xref graph`.
    XrefCall,
}

/// A directed dependency edge between two modules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepEdge {
    /// The module the dependency is declared in.
    pub from: NodeId,
    /// The module depended on.
    pub to: NodeId,
    /// How the dependency was declared/resolved.
    pub kind: DepKind,
}

/// The full project graph: every module node, the dependency edges between
/// them, and the top-level roots. Flat and serde-friendly by design -- this
/// struct IS the `--dump json` payload. Any petgraph-based structure needed
/// for layout/focus algorithms is built on demand from this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectGraph {
    /// Every node, keyed by id.
    pub nodes: HashMap<NodeId, ModuleNode>,
    /// Top-level node ids (no parent), stored sorted by display name
    /// (tie-broken by id) -- see [`ProjectGraph::sorted_roots`].
    pub roots: Vec<NodeId>,
    /// Every dependency edge in the graph.
    pub edges: Vec<DepEdge>,
}

impl ProjectGraph {
    /// Look up a node by id.
    pub fn node(&self, id: &NodeId) -> Option<&ModuleNode> {
        self.nodes.get(id)
    }

    /// `parent`'s children, sorted by `display_name`. This defines sibling
    /// order for navigation (`j`/`k` in [`crate::core::focus`]). Returns an
    /// empty vec if `parent` is unknown or has no children.
    pub fn sorted_children(&self, parent: &NodeId) -> Vec<NodeId> {
        let Some(node) = self.node(parent) else {
            return Vec::new();
        };
        self.sorted_by_name(&node.children)
    }

    /// The graph's top-level roots, sorted by `display_name` -- the sibling
    /// order navigation uses when the focused node has no parent.
    pub fn sorted_roots(&self) -> Vec<NodeId> {
        self.sorted_by_name(&self.roots)
    }

    /// Walk `id`'s parent chain up to its top-level ancestor (the node with
    /// no parent), returning that root's id. The result may itself be a
    /// synthetic namespace node ([`ModuleNode::files`] empty) -- that's
    /// fine, it's only ever used as a grouping/coloring key (see
    /// [`crate::graph::layers`] and [`crate::ui::theme::root_hue_color`]),
    /// never drawn or navigated to directly. Returns `id` itself if it's
    /// unknown or already a root.
    pub fn top_level_root(&self, id: &NodeId) -> NodeId {
        let mut current = id.clone();
        loop {
            match self.node(&current).and_then(|n| n.parent.clone()) {
                Some(parent) => current = parent,
                None => return current,
            }
        }
    }

    /// True if `id` belongs to a web namespace (Phoenix's `lib/*_web/`
    /// convention and the `FooWeb` module namespace it maps to) -- see
    /// [`crate::graph::layers`], which uses this to pin web modules to the
    /// top of the layered graph so it reads controller -> context -> schema
    /// like a call stack. Two independent signals, either sufficient:
    /// - any of `id`'s backing files sits under a directory component
    ///   ending in `_web` (Phoenix's `lib/<app>_web/` convention, including
    ///   umbrella apps like `lib/my_app_web/apps/...`);
    /// - `id`'s top-level namespace root (see [`Self::top_level_root`]) has
    ///   a `display_name` ending in `Web` (e.g. `MyAppWeb`).
    ///
    /// Always `false` for an unknown id, and for graphs with no Elixir-style
    /// web namespace at all (Rust crates, plain file trees) -- neither
    /// signal ever fires there.
    pub fn is_web_node(&self, id: &NodeId) -> bool {
        let Some(node) = self.node(id) else {
            return false;
        };

        let file_signals_web = node.files.iter().any(|file| {
            file.path
                .components()
                .any(|c| c.as_os_str().to_str().is_some_and(|s| s.ends_with("_web")))
        });
        if file_signals_web {
            return true;
        }

        let root = self.top_level_root(id);
        self.node(&root)
            .is_some_and(|root_node| root_node.display_name.ends_with("Web"))
    }

    /// Sort a slice of node ids by their `display_name`, dropping any id
    /// that isn't present in `nodes`.
    fn sorted_by_name(&self, ids: &[NodeId]) -> Vec<NodeId> {
        let mut named: Vec<(&str, NodeId)> = ids
            .iter()
            .filter_map(|id| Some((self.node(id)?.display_name.as_str(), id.clone())))
            .collect();
        named.sort_by(|a, b| a.0.cmp(b.0));
        named.into_iter().map(|(_, id)| id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built 3-node graph: one root with two children, one dep edge from
    /// one child to the other. Asserts the JSON round-trips and that
    /// `sorted_children` orders by `display_name`, not insertion order.
    #[test]
    fn round_trips_and_sorts_children_by_name() {
        let root_id = NodeId::from("app");
        let zeta_id = NodeId::from("app::zeta");
        let alpha_id = NodeId::from("app::alpha");

        let mut nodes = std::collections::HashMap::new();
        nodes.insert(
            root_id.clone(),
            ModuleNode {
                id: root_id.clone(),
                display_name: "app".to_string(),
                parent: None,
                children: vec![zeta_id.clone(), alpha_id.clone()],
                status: GitStatus::Unchanged,
                files: vec![],
            },
        );
        nodes.insert(
            zeta_id.clone(),
            ModuleNode {
                id: zeta_id.clone(),
                display_name: "zeta".to_string(),
                parent: Some(root_id.clone()),
                children: vec![],
                status: GitStatus::Modified,
                files: vec![FileRef {
                    path: PathBuf::from("src/zeta.rs"),
                    base_blob: Some("aaa111".to_string()),
                    head_blob: Some("bbb222".to_string()),
                }],
            },
        );
        nodes.insert(
            alpha_id.clone(),
            ModuleNode {
                id: alpha_id.clone(),
                display_name: "alpha".to_string(),
                parent: Some(root_id.clone()),
                children: vec![],
                status: GitStatus::Added,
                files: vec![FileRef {
                    path: PathBuf::from("src/alpha.rs"),
                    base_blob: None,
                    head_blob: Some("ccc333".to_string()),
                }],
            },
        );

        let graph = ProjectGraph {
            nodes,
            roots: vec![root_id.clone()],
            edges: vec![DepEdge {
                from: zeta_id.clone(),
                to: alpha_id.clone(),
                kind: DepKind::Use,
            }],
        };

        // Sibling order is name-sorted, not insertion order (zeta was
        // inserted into `children` before alpha).
        assert_eq!(
            graph.sorted_children(&root_id),
            vec![alpha_id.clone(), zeta_id.clone()]
        );
        assert_eq!(graph.sorted_roots(), vec![root_id.clone()]);
        assert_eq!(graph.node(&zeta_id).unwrap().display_name, "zeta");
        assert_eq!(graph.node(&NodeId::from("missing")), None);

        let json = serde_json::to_string(&graph).expect("serialize");
        let round_tripped: ProjectGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, graph);
    }

    #[test]
    fn node_id_display_and_conversions() {
        let from_str: NodeId = "foo::bar".into();
        let from_string: NodeId = String::from("foo::bar").into();
        assert_eq!(from_str, from_string);
        assert_eq!(from_str.to_string(), "foo::bar");
    }

    fn leaf_node(id: &str, parent: Option<&str>, path: &str) -> (NodeId, ModuleNode) {
        let node_id = NodeId::from(id);
        (
            node_id.clone(),
            ModuleNode {
                id: node_id,
                display_name: id.rsplit('.').next().unwrap_or(id).to_string(),
                parent: parent.map(NodeId::from),
                children: vec![],
                status: GitStatus::Unchanged,
                files: vec![FileRef {
                    path: PathBuf::from(path),
                    base_blob: Some("b".to_string()),
                    head_blob: Some("h".to_string()),
                }],
            },
        )
    }

    fn namespace_root(id: &str, display_name: &str, children: &[&str]) -> (NodeId, ModuleNode) {
        let node_id = NodeId::from(id);
        (
            node_id.clone(),
            ModuleNode {
                id: node_id,
                display_name: display_name.to_string(),
                parent: None,
                children: children.iter().map(|c| NodeId::from(*c)).collect(),
                status: GitStatus::Unchanged,
                files: vec![],
            },
        )
    }

    #[test]
    fn is_web_node_true_when_top_level_root_name_ends_in_web() {
        let root_id = NodeId::from("elixir:MyAppWeb");
        let leaf_id = "elixir:MyAppWeb.PageController";
        let (_, root_node) = namespace_root("elixir:MyAppWeb", "MyAppWeb", &[leaf_id]);
        let (_, leaf) = leaf_node(
            leaf_id,
            Some("elixir:MyAppWeb"),
            "lib/my_app_web/controllers/page_controller.ex",
        );
        let graph = ProjectGraph {
            nodes: [(root_id.clone(), root_node), (NodeId::from(leaf_id), leaf)]
                .into_iter()
                .collect(),
            roots: vec![root_id],
            edges: vec![],
        };

        assert!(graph.is_web_node(&NodeId::from(leaf_id)));
    }

    #[test]
    fn is_web_node_true_when_file_path_is_under_a_web_directory_even_without_web_root_name() {
        // Root display name doesn't end in "Web", but the backing file
        // lives under a `lib/*_web/` directory -- the file signal alone
        // should be enough.
        let root_id = NodeId::from("elixir:MyApp");
        let leaf_id = "elixir:MyApp.PageController";
        let (_, root_node) = namespace_root("elixir:MyApp", "MyApp", &[leaf_id]);
        let (_, leaf) = leaf_node(
            leaf_id,
            Some("elixir:MyApp"),
            "lib/my_app_web/controllers/page_controller.ex",
        );
        let graph = ProjectGraph {
            nodes: [(root_id.clone(), root_node), (NodeId::from(leaf_id), leaf)]
                .into_iter()
                .collect(),
            roots: vec![root_id],
            edges: vec![],
        };

        assert!(graph.is_web_node(&NodeId::from(leaf_id)));
    }

    #[test]
    fn is_web_node_false_for_plain_module_with_no_web_signal() {
        let root_id = NodeId::from("elixir:MyApp");
        let leaf_id = "elixir:MyApp.Accounts.User";
        let (_, root_node) = namespace_root("elixir:MyApp", "MyApp", &[leaf_id]);
        let (_, leaf) = leaf_node(leaf_id, Some("elixir:MyApp"), "lib/my_app/accounts/user.ex");
        let graph = ProjectGraph {
            nodes: [(root_id.clone(), root_node), (NodeId::from(leaf_id), leaf)]
                .into_iter()
                .collect(),
            roots: vec![root_id],
            edges: vec![],
        };

        assert!(!graph.is_web_node(&NodeId::from(leaf_id)));
    }

    #[test]
    fn is_web_node_false_for_unknown_id() {
        let graph = ProjectGraph {
            nodes: HashMap::new(),
            roots: vec![],
            edges: vec![],
        };
        assert!(!graph.is_web_node(&NodeId::from("missing")));
    }
}
