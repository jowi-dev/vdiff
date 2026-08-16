//! Fold every extracted source file into a [`ProjectGraph`]: compute each
//! module's [`NodeId`] and its place in the hierarchy per language
//! convention, map git status in via the diff's [`ChangeSet`], and resolve
//! cross-file dependency edges.
//!
//! NodeId conventions:
//! - Rust: `rust:<crate>::<mod path>`, derived from the file's location
//!   (crate name = the directory containing `src/`; `lib.rs`/`main.rs` is
//!   the crate root, `foo.rs`/`foo/mod.rs` is `foo`, `foo/bar.rs` is
//!   `foo::bar`) joined with any inline `mod` nesting from extraction.
//! - Elixir: `elixir:<dotted name>`, the module's full dotted name exactly
//!   as `defmodule` wrote it.
//! - Other files: `file:<repo-relative path>`, forward-slash-joined.
//!
//! Every id carries its language namespace prefix (`rust:`/`elixir:`/
//! `file:`) so that, e.g., a single-segment Rust crate (`Foo/` with no
//! nesting) and an Elixir `defmodule Foo` never collide into one node --
//! ids are opaque to consumers, [`ModuleNode::display_name`] carries the
//! human-readable label, so the prefix is contract-safe. Every id
//! construction site (module ids, synthetic ancestors, and edge endpoints
//! resolved in [`crate::pipeline::resolve`]) must agree on this scheme.
//!
//! Hierarchy: a node's parent is everything before its id's last
//! separator-delimited segment. Any ancestor that isn't itself a real,
//! extracted node (a crate root with no `lib.rs`/`main.rs` seen, an Elixir
//! namespace prefix with no matching `defmodule`, a directory) is
//! synthesized as an [`GitStatus::Unchanged`] node with no files -- a real
//! definition for that same id always wins, since synthesis only ever
//! fills in a gap, never overwrites.

use std::collections::HashMap;
use std::path::Path;

use crate::graph::model::{FileRef, GitStatus, ModuleNode, NodeId, ProjectGraph};
use crate::pipeline::changed_files::ChangeSet;
use crate::pipeline::extract::ModuleDef;
use crate::pipeline::resolve::{resolve_edges, NodeContext};

/// Which NodeId/hierarchy convention a file's modules follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Elixir,
    /// A non-code file. `defs` should be exactly one empty-name
    /// [`ModuleDef`] with no dep_refs, representing the whole file as a
    /// leaf node.
    Other,
}

/// One source file's extracted modules, ready to fold into a
/// [`ProjectGraph`].
pub struct FileInput {
    pub file_ref: FileRef,
    pub lang: Lang,
    pub defs: Vec<ModuleDef>,
}

/// The separator a node id's segments are joined with, and so the rule for
/// splitting off its last segment (display name) and everything before it
/// (parent id). Also carries the language namespace prefix every id in
/// that scheme starts with (see the module-level NodeId conventions
/// above).
#[derive(Clone, Copy)]
enum Sep {
    DoubleColon,
    Dot,
    Slash,
}

impl Sep {
    fn as_str(self) -> &'static str {
        match self {
            Sep::DoubleColon => "::",
            Sep::Dot => ".",
            Sep::Slash => "/",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Sep::DoubleColon => "rust:",
            Sep::Dot => "elixir:",
            Sep::Slash => "file:",
        }
    }
}

/// Build a [`ProjectGraph`] from every extracted file plus the diff's
/// [`ChangeSet`].
pub fn build(files: Vec<FileInput>, changes: &ChangeSet) -> ProjectGraph {
    let mut nodes: HashMap<NodeId, ModuleNode> = HashMap::new();
    let mut ancestor_work: Vec<(NodeId, Sep)> = Vec::new();
    let mut contexts: Vec<NodeContext> = Vec::new();

    for file in files {
        let status = changes.status_for(&file.file_ref.path);
        match file.lang {
            Lang::Rust => {
                let (crate_name, file_prefix) = rust_crate_and_prefix(&file.file_ref.path);
                for def in file.defs {
                    let full_path = join_segments(&file_prefix, &def.name, "::");
                    let id = rust_node_id(&crate_name, &full_path);
                    insert_real_node(
                        &mut nodes,
                        &id,
                        Sep::DoubleColon,
                        status,
                        file.file_ref.clone(),
                    );
                    ancestor_work.push((id.clone(), Sep::DoubleColon));
                    contexts.push(NodeContext {
                        id,
                        dep_refs: def.dep_refs,
                        rust_crate_name: Some(crate_name.clone()),
                    });
                }
            }
            Lang::Elixir => {
                for def in file.defs {
                    let id = elixir_node_id(&def.name);
                    insert_real_node(&mut nodes, &id, Sep::Dot, status, file.file_ref.clone());
                    ancestor_work.push((id.clone(), Sep::Dot));
                    contexts.push(NodeContext {
                        id,
                        dep_refs: def.dep_refs,
                        rust_crate_name: None,
                    });
                }
            }
            Lang::Other => {
                let id = other_node_id(&file.file_ref.path);
                insert_real_node(&mut nodes, &id, Sep::Slash, status, file.file_ref.clone());
                ancestor_work.push((id, Sep::Slash));
            }
        }
    }

    for (id, sep) in ancestor_work {
        ensure_ancestors(&mut nodes, &id, sep);
    }

    let roots = link_children_and_collect_roots(&mut nodes);
    let edges = resolve_edges(&nodes, &contexts);

    ProjectGraph {
        nodes,
        roots,
        edges,
    }
}

/// Insert (or, if `id` was already inserted by an earlier def in the same
/// file, merge into) a real node.
fn insert_real_node(
    nodes: &mut HashMap<NodeId, ModuleNode>,
    id: &NodeId,
    sep: Sep,
    status: GitStatus,
    file_ref: FileRef,
) {
    if let Some(existing) = nodes.get_mut(id) {
        existing.files.push(file_ref);
        existing.status = combine_status(existing.status, status);
        return;
    }
    let (parent, display_name) = split_prefixed_id(&id.to_string(), sep);
    nodes.insert(
        id.clone(),
        ModuleNode {
            id: id.clone(),
            display_name,
            parent: parent.map(NodeId::from),
            children: Vec::new(),
            status,
            files: vec![file_ref],
        },
    );
}

/// Combine two statuses observed for the same node: Modified beats
/// everything else. A node that's both Deleted and Added -- e.g. a moved
/// module whose git rename detection missed, reported as a Deleted
/// old-location file plus an Added new-location file both resolving to
/// the same node id -- exists at both base and head under this id, so
/// that combination is itself Modified, not Deleted. Otherwise Deleted
/// beats Added beats Unchanged.
fn combine_status(a: GitStatus, b: GitStatus) -> GitStatus {
    use GitStatus::{Added, Deleted, Modified, Unchanged};
    match (a, b) {
        (Modified, _) | (_, Modified) => Modified,
        (Deleted, Added) | (Added, Deleted) => Modified,
        (Deleted, _) | (_, Deleted) => Deleted,
        (Added, _) | (_, Added) => Added,
        (Unchanged, Unchanged) => Unchanged,
    }
}

/// Walk up from `start`'s parent chain, inserting a synthetic
/// [`GitStatus::Unchanged`] node for any ancestor id that isn't already
/// present. Stops as soon as it reaches an ancestor that already exists --
/// that ancestor's own chain is (or will be) ensured via its own entry in
/// `ancestor_work`.
fn ensure_ancestors(nodes: &mut HashMap<NodeId, ModuleNode>, start: &NodeId, sep: Sep) {
    let mut current_parent = nodes.get(start).and_then(|n| n.parent.clone());
    while let Some(parent_id) = current_parent {
        if nodes.contains_key(&parent_id) {
            break;
        }
        let (grandparent, display_name) = split_prefixed_id(&parent_id.to_string(), sep);
        nodes.insert(
            parent_id.clone(),
            ModuleNode {
                id: parent_id.clone(),
                display_name,
                parent: grandparent.clone().map(NodeId::from),
                children: Vec::new(),
                status: GitStatus::Unchanged,
                files: Vec::new(),
            },
        );
        current_parent = grandparent.map(NodeId::from);
    }
}

/// Populate every node's `children` from the reverse of its `parent`, and
/// return the ids with no parent (the graph's roots). Both `children` and
/// the returned roots are sorted by display name (tie-broken by id) before
/// returning -- `HashMap` iteration order is otherwise unspecified per
/// process, which made `--dump json` a non-deterministic machine contract
/// for identical repo states. [`ProjectGraph::sorted_children`]/
/// [`ProjectGraph::sorted_roots`] remain available and now simply agree
/// with this stored order.
fn link_children_and_collect_roots(nodes: &mut HashMap<NodeId, ModuleNode>) -> Vec<NodeId> {
    let mut roots = Vec::new();
    let child_parent_pairs: Vec<(NodeId, Option<NodeId>)> = nodes
        .values()
        .map(|n| (n.id.clone(), n.parent.clone()))
        .collect();
    for (child, parent) in child_parent_pairs {
        match parent {
            Some(p) => {
                if let Some(parent_node) = nodes.get_mut(&p) {
                    parent_node.children.push(child);
                }
            }
            None => roots.push(child),
        }
    }

    let names: HashMap<NodeId, String> = nodes
        .values()
        .map(|n| (n.id.clone(), n.display_name.clone()))
        .collect();
    for node in nodes.values_mut() {
        sort_ids_by_name(&names, &mut node.children);
    }
    sort_ids_by_name(&names, &mut roots);
    roots
}

/// Sort `ids` by their looked-up display name, tie-broken by id itself so
/// the order is fully deterministic even for same-named siblings.
fn sort_ids_by_name(names: &HashMap<NodeId, String>, ids: &mut [NodeId]) {
    ids.sort_by(|a, b| {
        let name_a = names.get(a).map(String::as_str).unwrap_or_default();
        let name_b = names.get(b).map(String::as_str).unwrap_or_default();
        name_a.cmp(name_b).then_with(|| a.cmp(b))
    });
}

/// Split `id` into its parent id (everything before the last `sep`) and
/// its display name (the last segment). `None` parent if `id` has no `sep`
/// at all (a root).
fn split_parent(id: &str, sep: Sep) -> (Option<String>, String) {
    match id.rsplit_once(sep.as_str()) {
        Some((parent, last)) => (Some(parent.to_string()), last.to_string()),
        None => (None, id.to_string()),
    }
}

/// [`split_parent`], but for a full (already language-prefixed) id: the
/// prefix is stripped before splitting so it never leaks into
/// `display_name`, then reattached to the parent id so the whole ancestor
/// chain stays in the same language namespace.
fn split_prefixed_id(id: &str, sep: Sep) -> (Option<String>, String) {
    let prefix = sep.prefix();
    let body = id.strip_prefix(prefix).unwrap_or(id);
    let (parent_body, display_name) = split_parent(body, sep);
    (
        parent_body.map(|body| format!("{prefix}{body}")),
        display_name,
    )
}

fn join_segments(a: &str, b: &str, sep: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (false, true) => a.to_string(),
        (false, false) => format!("{a}{sep}{b}"),
    }
}

fn rust_node_id(crate_name: &str, full_path: &str) -> NodeId {
    if full_path.is_empty() {
        NodeId::from(format!("rust:{crate_name}"))
    } else {
        NodeId::from(format!("rust:{crate_name}::{full_path}"))
    }
}

fn elixir_node_id(dotted_name: &str) -> NodeId {
    NodeId::from(format!("elixir:{dotted_name}"))
}

fn other_node_id(path: &Path) -> NodeId {
    NodeId::from(format!("file:{}", path_to_id(path)))
}

/// Best-effort (crate_name, module_path_prefix) for a Rust source file's
/// repo-relative path. The crate root is the directory containing this
/// file's `src/` component; falls back to the literal crate name
/// `"crate"` if no `src/` component is found at all (e.g. a single-crate
/// repo laid out without an enclosing crate directory) -- a known,
/// documented limitation rather than a Cargo.toml-driven lookup.
fn rust_crate_and_prefix(path: &Path) -> (String, String) {
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let Some(src_index) = components.iter().position(|c| *c == "src") else {
        return ("crate".to_string(), String::new());
    };
    let crate_name = if src_index == 0 {
        "crate".to_string()
    } else {
        components[src_index - 1].to_string()
    };
    let prefix = module_path_from_components(&components[src_index + 1..]);
    (crate_name, prefix)
}

/// `["foo", "bar.rs"]` -> `"foo::bar"`; `["lib.rs"]` -> `""`;
/// `["foo", "mod.rs"]` -> `"foo"`.
fn module_path_from_components(components: &[&str]) -> String {
    let Some((last, dirs)) = components.split_last() else {
        return String::new();
    };
    let stem = last.strip_suffix(".rs").unwrap_or(last);
    let mut segments: Vec<&str> = dirs.to_vec();
    if stem != "lib" && stem != "main" && stem != "mod" {
        segments.push(stem);
    }
    segments.join("::")
}

/// Forward-slash-joined repo-relative path, used as the NodeId for
/// [`Lang::Other`] files regardless of the host OS's path separator.
fn path_to_id(path: &Path) -> String {
    path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::DepKind;
    use crate::pipeline::extract::DepRef;
    use crate::pipeline::repo::{Change, FileDelta};
    use std::path::PathBuf;

    fn file_ref(path: &str) -> FileRef {
        FileRef {
            path: PathBuf::from(path),
            base_blob: Some("base".to_string()),
            head_blob: Some("head".to_string()),
        }
    }

    fn module(name: &str, dep_refs: Vec<DepRef>) -> ModuleDef {
        ModuleDef {
            name: name.to_string(),
            dep_refs,
        }
    }

    fn dep(name: &str, kind: DepKind) -> DepRef {
        DepRef {
            name: name.to_string(),
            kind,
        }
    }

    fn changes(deltas: Vec<FileDelta>) -> ChangeSet {
        ChangeSet::from_deltas(deltas)
    }

    #[test]
    fn rust_crate_and_prefix_covers_layout_variants() {
        assert_eq!(
            rust_crate_and_prefix(Path::new("crate_a/src/lib.rs")),
            ("crate_a".to_string(), "".to_string())
        );
        assert_eq!(
            rust_crate_and_prefix(Path::new("crate_a/src/main.rs")),
            ("crate_a".to_string(), "".to_string())
        );
        assert_eq!(
            rust_crate_and_prefix(Path::new("crate_a/src/foo.rs")),
            ("crate_a".to_string(), "foo".to_string())
        );
        assert_eq!(
            rust_crate_and_prefix(Path::new("crate_a/src/foo/mod.rs")),
            ("crate_a".to_string(), "foo".to_string())
        );
        assert_eq!(
            rust_crate_and_prefix(Path::new("crate_a/src/foo/bar.rs")),
            ("crate_a".to_string(), "foo::bar".to_string())
        );
    }

    #[test]
    fn rust_cross_file_edge_resolves_via_crate_prefix() {
        let files = vec![
            FileInput {
                file_ref: file_ref("crate_a/src/lib.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![dep("crate::foo::Thing", DepKind::Use)])],
            },
            FileInput {
                file_ref: file_ref("crate_a/src/foo.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
        ];
        let graph = build(files, &changes(vec![]));

        assert!(graph.nodes.contains_key(&NodeId::from("rust:crate_a")));
        assert!(graph.nodes.contains_key(&NodeId::from("rust:crate_a::foo")));
        assert_eq!(graph.roots, vec![NodeId::from("rust:crate_a")]);
        assert_eq!(
            graph
                .node(&NodeId::from("rust:crate_a::foo"))
                .unwrap()
                .parent,
            Some(NodeId::from("rust:crate_a"))
        );
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from, NodeId::from("rust:crate_a"));
        assert_eq!(graph.edges[0].to, NodeId::from("rust:crate_a::foo"));
    }

    #[test]
    fn rust_missing_crate_root_is_synthesized() {
        // Only foo/bar.rs is fed in (simulating a gap in the tracked-file
        // set); crate_a's root has no lib.rs/main.rs def of its own here,
        // so it must be synthesized as an Unchanged, fileless node.
        let files = vec![FileInput {
            file_ref: file_ref("crate_a/src/foo/bar.rs"),
            lang: Lang::Rust,
            defs: vec![module("", vec![])],
        }];
        let graph = build(files, &changes(vec![]));

        let root = graph
            .node(&NodeId::from("rust:crate_a"))
            .expect("synthesized root");
        assert_eq!(root.status, GitStatus::Unchanged);
        assert!(root.files.is_empty());
        let foo = graph
            .node(&NodeId::from("rust:crate_a::foo"))
            .expect("synthesized intermediate dir node");
        assert_eq!(foo.status, GitStatus::Unchanged);
        assert_eq!(foo.parent, Some(NodeId::from("rust:crate_a")));
        assert_eq!(
            graph
                .node(&NodeId::from("rust:crate_a::foo::bar"))
                .unwrap()
                .parent,
            Some(NodeId::from("rust:crate_a::foo"))
        );
    }

    #[test]
    fn elixir_cross_file_alias_edge_and_synthetic_namespace() {
        let files = vec![
            FileInput {
                file_ref: file_ref("lib/my_app/accounts.ex"),
                lang: Lang::Elixir,
                defs: vec![module(
                    "MyApp.Accounts",
                    vec![dep("MyApp.Repo", DepKind::Alias)],
                )],
            },
            FileInput {
                file_ref: file_ref("lib/my_app/repo.ex"),
                lang: Lang::Elixir,
                defs: vec![module("MyApp.Repo", vec![])],
            },
        ];
        let graph = build(files, &changes(vec![]));

        // "MyApp" is referenced by both dotted names but never itself
        // `defmodule`d -- it must be synthesized.
        let namespace = graph
            .node(&NodeId::from("elixir:MyApp"))
            .expect("synthesized namespace node");
        assert_eq!(namespace.status, GitStatus::Unchanged);
        assert!(namespace.files.is_empty());
        assert_eq!(graph.roots, vec![NodeId::from("elixir:MyApp")]);

        assert!(graph.edges.contains(&crate::graph::model::DepEdge {
            from: NodeId::from("elixir:MyApp.Accounts"),
            to: NodeId::from("elixir:MyApp.Repo"),
            kind: DepKind::Alias,
        }));
    }

    #[test]
    fn real_defmodule_takes_precedence_over_synthetic_namespace() {
        let files = vec![
            FileInput {
                file_ref: file_ref("lib/my_app.ex"),
                lang: Lang::Elixir,
                defs: vec![module("MyApp", vec![])],
            },
            FileInput {
                file_ref: file_ref("lib/my_app/accounts/user.ex"),
                lang: Lang::Elixir,
                defs: vec![module("MyApp.Accounts.User", vec![])],
            },
        ];
        let deltas = vec![FileDelta {
            path: PathBuf::from("lib/my_app.ex"),
            change: Change::Modified,
        }];
        let graph = build(files, &changes(deltas));

        let my_app = graph.node(&NodeId::from("elixir:MyApp")).unwrap();
        assert_eq!(
            my_app.status,
            GitStatus::Modified,
            "real def's status must not be clobbered by ancestor synthesis"
        );
        assert_eq!(my_app.files.len(), 1);
    }

    #[test]
    fn other_files_nest_by_directory_with_synthetic_dir_nodes() {
        let files = vec![
            FileInput {
                file_ref: file_ref("docs/a.md"),
                lang: Lang::Other,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("docs/sub/b.md"),
                lang: Lang::Other,
                defs: vec![module("", vec![])],
            },
        ];
        let graph = build(files, &changes(vec![]));

        let docs = graph
            .node(&NodeId::from("file:docs"))
            .expect("synthetic dir node");
        assert_eq!(docs.status, GitStatus::Unchanged);
        assert!(docs.files.is_empty());
        let sub = graph
            .node(&NodeId::from("file:docs/sub"))
            .expect("synthetic nested dir node");
        assert_eq!(sub.parent, Some(NodeId::from("file:docs")));
        assert_eq!(
            graph.node(&NodeId::from("file:docs/a.md")).unwrap().parent,
            Some(NodeId::from("file:docs"))
        );
        assert_eq!(
            graph
                .node(&NodeId::from("file:docs/sub/b.md"))
                .unwrap()
                .parent,
            Some(NodeId::from("file:docs/sub"))
        );
        assert_eq!(graph.roots, vec![NodeId::from("file:docs")]);
    }

    #[test]
    fn cross_language_single_segment_ids_do_not_collide() {
        // A single-segment Rust crate id (`Foo/` with no `src/` nesting
        // under it, so the module path is just the crate name) and an
        // Elixir `defmodule Foo` both produce the bare display name "Foo".
        // Without a language namespace prefix these collide into one node
        // with merged files/edges; they must land as two distinct nodes.
        let files = vec![
            FileInput {
                file_ref: file_ref("Foo/src/lib.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("lib/foo.ex"),
                lang: Lang::Elixir,
                defs: vec![module("Foo", vec![])],
            },
        ];
        let deltas = vec![
            FileDelta {
                path: PathBuf::from("Foo/src/lib.rs"),
                change: Change::Modified,
            },
            FileDelta {
                path: PathBuf::from("lib/foo.ex"),
                change: Change::Added,
            },
        ];
        let graph = build(files, &changes(deltas));

        let rust_node = graph
            .node(&NodeId::from("rust:Foo"))
            .expect("rust-namespaced Foo node");
        let elixir_node = graph
            .node(&NodeId::from("elixir:Foo"))
            .expect("elixir-namespaced Foo node");
        assert_ne!(rust_node.id, elixir_node.id);
        assert_eq!(rust_node.status, GitStatus::Modified);
        assert_eq!(elixir_node.status, GitStatus::Added);
        assert_eq!(rust_node.files.len(), 1);
        assert_eq!(elixir_node.files.len(), 1);
    }

    #[test]
    fn status_mapping_from_changeset() {
        let files = vec![
            FileInput {
                file_ref: file_ref("crate_a/src/lib.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("crate_a/src/added.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("crate_a/src/deleted.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
        ];
        let deltas = vec![
            FileDelta {
                path: PathBuf::from("crate_a/src/lib.rs"),
                change: Change::Modified,
            },
            FileDelta {
                path: PathBuf::from("crate_a/src/added.rs"),
                change: Change::Added,
            },
            FileDelta {
                path: PathBuf::from("crate_a/src/deleted.rs"),
                change: Change::Deleted,
            },
        ];
        let graph = build(files, &changes(deltas));

        assert_eq!(
            graph.node(&NodeId::from("rust:crate_a")).unwrap().status,
            GitStatus::Modified
        );
        assert_eq!(
            graph
                .node(&NodeId::from("rust:crate_a::added"))
                .unwrap()
                .status,
            GitStatus::Added
        );
        assert_eq!(
            graph
                .node(&NodeId::from("rust:crate_a::deleted"))
                .unwrap()
                .status,
            GitStatus::Deleted
        );
    }

    #[test]
    fn module_moved_without_rename_detection_is_modified_not_deleted() {
        // Rename detection missed a heavily-edited move: git reports it as
        // a Deleted old-location file plus an Added new-location file, but
        // both land on the same node id (crate_a::foo, whether split from
        // foo.rs or foo/mod.rs). The module still exists at HEAD, so the
        // combined status must be Modified, not Deleted.
        let files = vec![
            FileInput {
                file_ref: file_ref("crate_a/src/foo.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("crate_a/src/foo/mod.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
        ];
        let deltas = vec![
            FileDelta {
                path: PathBuf::from("crate_a/src/foo.rs"),
                change: Change::Deleted,
            },
            FileDelta {
                path: PathBuf::from("crate_a/src/foo/mod.rs"),
                change: Change::Added,
            },
        ];
        let graph = build(files, &changes(deltas));

        assert_eq!(
            graph
                .node(&NodeId::from("rust:crate_a::foo"))
                .unwrap()
                .status,
            GitStatus::Modified
        );
    }

    #[test]
    fn stored_children_and_roots_are_sorted_by_display_name() {
        // Insertion order here (zeta, mid, alpha for crate_a's children;
        // zzz_crate then crate_a for the roots) is deliberately not
        // name-sorted, so this only passes if `build` itself sorts
        // `children`/`roots` rather than relying on HashMap iteration
        // order, which is what made `--dump json` output
        // non-deterministic between runs of the same repo state.
        let files = vec![
            FileInput {
                file_ref: file_ref("crate_a/src/lib.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("crate_a/src/zeta.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("crate_a/src/mid.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("crate_a/src/alpha.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
            FileInput {
                file_ref: file_ref("zzz_crate/src/lib.rs"),
                lang: Lang::Rust,
                defs: vec![module("", vec![])],
            },
        ];
        let graph = build(files, &changes(vec![]));

        let crate_a = graph.node(&NodeId::from("rust:crate_a")).unwrap();
        assert_eq!(
            crate_a.children,
            vec![
                NodeId::from("rust:crate_a::alpha"),
                NodeId::from("rust:crate_a::mid"),
                NodeId::from("rust:crate_a::zeta"),
            ],
            "stored children order must already be display-name order"
        );
        assert_eq!(
            graph.roots,
            vec![NodeId::from("rust:crate_a"), NodeId::from("rust:zzz_crate"),],
            "stored roots order must already be display-name order"
        );
    }
}
