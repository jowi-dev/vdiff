//! Build a [`FunctionIndex`] for a repo's change set: the impure glue that
//! picks which Elixir modules are in scope, extracts their functions/call
//! sites via [`extract_functions`], resolves calls into [`FunctionEdge`]s,
//! and filters the result down to a small "neighborhood" around what
//! actually changed. See [`build_function_index`] for the full algorithm.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::graph::functions::{
    function_node_id, is_function_id, FunctionEdge, FunctionIndex, FunctionInfo,
};
use crate::graph::model::{FileRef, GitStatus, NodeId, ProjectGraph};
use crate::pipeline::error::Result;
use crate::pipeline::extract::elixir_extract::{extract_functions, RawCallTarget, RawFunctionDef};
use crate::pipeline::file_diff::{changed_head_ranges, load_file_diff};
use crate::pipeline::repo::GitRepo;
use crate::pipeline::resolve::resolve_elixir_module;

/// True if `id`'s string form starts with the `elixir:` language namespace
/// prefix (see [`crate::graph::model::NodeId`]'s doc for the convention).
fn is_elixir_id(id: &NodeId) -> bool {
    id.to_string().starts_with("elixir:")
}

/// True if `path` has a `.ex`/`.exs` extension.
fn is_elixir_path(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("ex" | "exs")
    )
}

/// Step 1 of [`build_function_index`]'s algorithm: every Elixir module in
/// `graph` whose status isn't [`GitStatus::Unchanged`], plus every Elixir
/// module one module-edge hop away from one of those (either direction),
/// excluding modules with no backing files either way.
fn target_module_ids(graph: &ProjectGraph) -> HashSet<NodeId> {
    let has_files = |id: &NodeId| graph.node(id).is_some_and(|n| !n.files.is_empty());

    let changed: HashSet<NodeId> = graph
        .nodes
        .values()
        .filter(|n| is_elixir_id(&n.id) && n.status != GitStatus::Unchanged && !n.files.is_empty())
        .map(|n| n.id.clone())
        .collect();

    let mut targets = changed.clone();
    for edge in &graph.edges {
        if changed.contains(&edge.from) && is_elixir_id(&edge.to) && has_files(&edge.to) {
            targets.insert(edge.to.clone());
        }
        if changed.contains(&edge.to) && is_elixir_id(&edge.from) && has_files(&edge.from) {
            targets.insert(edge.from.clone());
        }
    }
    targets
}

/// One extracted module's function defs/call sites, plus the head-line
/// ranges the diff touched in the file it came from -- the pure input to
/// [`build_candidates`]/[`build_edges`].
struct ExtractedModule {
    node_id: NodeId,
    functions: Vec<RawFunctionDef>,
    calls: Vec<crate::pipeline::extract::elixir_extract::RawCallSite>,
    changed_ranges: Vec<(usize, usize)>,
}

/// True if `range` (a function's `[start_row, end_row]`) intersects any of
/// `changed_ranges` (the diff's changed head-line ranges for that function's
/// file).
fn range_is_changed(range: (usize, usize), changed_ranges: &[(usize, usize)]) -> bool {
    changed_ranges
        .iter()
        .any(|&(cs, ce)| range.0 <= ce && cs <= range.1)
}

/// Step 4 of [`build_function_index`]'s algorithm: every extracted def
/// becomes a candidate [`FunctionInfo`], grouped by owning module id, marked
/// `changed` per [`range_is_changed`].
fn build_candidates(modules: &[ExtractedModule]) -> HashMap<NodeId, Vec<FunctionInfo>> {
    let mut out: HashMap<NodeId, Vec<FunctionInfo>> = HashMap::new();
    for module in modules {
        let entry = out.entry(module.node_id.clone()).or_default();
        for def in &module.functions {
            let id = function_node_id(&module.node_id, &def.name, def.arity);
            let changed = range_is_changed((def.start_row, def.end_row), &module.changed_ranges);
            entry.push(FunctionInfo {
                id,
                name: def.name.clone(),
                arity: def.arity,
                start_line: def.start_row,
                end_line: def.end_row,
                public: def.public,
                changed,
            });
        }
    }
    out
}

/// Resolve a call target's `(name, arity)` against a module's candidates:
/// an exact name+arity match if one exists, else the sole candidate sharing
/// `name` if exactly one does (covers default-arg heads, whose clauses can
/// disagree on arity), else `None`.
fn resolve_in_candidates<'a>(
    name: &str,
    arity: usize,
    candidates: &'a [FunctionInfo],
) -> Option<&'a FunctionInfo> {
    if let Some(exact) = candidates
        .iter()
        .find(|f| f.name == name && f.arity == arity)
    {
        return Some(exact);
    }
    let mut same_name = candidates.iter().filter(|f| f.name == name);
    let only = same_name.next()?;
    if same_name.next().is_none() {
        Some(only)
    } else {
        None
    }
}

/// Step 5 of [`build_function_index`]'s algorithm: turn every extracted
/// module's call sites into [`FunctionEdge`]s, resolving remote targets
/// through `resolve_module` (a closure so this stays pure/unit-testable
/// without a real [`ProjectGraph`]), deduped by `(from, to)`.
fn build_edges(
    modules: &[ExtractedModule],
    candidates: &HashMap<NodeId, Vec<FunctionInfo>>,
    resolve_module: impl Fn(&str) -> Option<NodeId>,
) -> Vec<FunctionEdge> {
    let mut edges = Vec::new();
    let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();

    for module in modules {
        let Some(own_candidates) = candidates.get(&module.node_id) else {
            continue;
        };
        for call in &module.calls {
            let (caller_name, caller_arity) = &call.caller;
            let Some(caller) = own_candidates
                .iter()
                .find(|f| &f.name == caller_name && f.arity == *caller_arity)
            else {
                continue;
            };

            let to = match &call.target {
                RawCallTarget::Local { name, arity } => {
                    match resolve_in_candidates(name, *arity, own_candidates) {
                        Some(callee) => callee.id.clone(),
                        None => continue,
                    }
                }
                RawCallTarget::Remote {
                    module: target_module,
                    name,
                    arity,
                } => {
                    let Some(target_id) = resolve_module(target_module) else {
                        continue;
                    };
                    match candidates
                        .get(&target_id)
                        .and_then(|c| resolve_in_candidates(name, *arity, c))
                    {
                        Some(callee) => callee.id.clone(),
                        None => target_id,
                    }
                }
            };

            let key = (caller.id.clone(), to.clone());
            if seen.insert(key) {
                edges.push(FunctionEdge {
                    from: caller.id.clone(),
                    to,
                });
            }
        }
    }

    edges
}

/// Step 6 of [`build_function_index`]'s algorithm: keep only functions that
/// are changed, or are the endpoint of a "relevant" edge (one whose `from`
/// is changed, or whose function-precise `to` is changed); keep only edges
/// that are relevant. Modules with no kept functions are omitted; kept
/// functions are sorted by `start_line`.
fn filter_to_neighborhood(
    candidates: HashMap<NodeId, Vec<FunctionInfo>>,
    edges: Vec<FunctionEdge>,
) -> FunctionIndex {
    let changed_ids: HashSet<NodeId> = candidates
        .values()
        .flatten()
        .filter(|f| f.changed)
        .map(|f| f.id.clone())
        .collect();

    let relevant_edges: Vec<FunctionEdge> = edges
        .into_iter()
        .filter(|e| {
            changed_ids.contains(&e.from) || (is_function_id(&e.to) && changed_ids.contains(&e.to))
        })
        .collect();

    let mut keep_ids = changed_ids;
    for edge in &relevant_edges {
        keep_ids.insert(edge.from.clone());
        if is_function_id(&edge.to) {
            keep_ids.insert(edge.to.clone());
        }
    }

    let mut functions: HashMap<NodeId, Vec<FunctionInfo>> = HashMap::new();
    for (module_id, infos) in candidates {
        let mut kept: Vec<FunctionInfo> = infos
            .into_iter()
            .filter(|f| keep_ids.contains(&f.id))
            .collect();
        if kept.is_empty() {
            continue;
        }
        kept.sort_by_key(|f| f.start_line);
        functions.insert(module_id, kept);
    }

    FunctionIndex {
        functions,
        edges: relevant_edges,
    }
}

/// Build a [`FunctionIndex`] for `repo`'s change set against `graph` (as
/// already built by [`crate::pipeline::build_graph`] for the same
/// `base_oid`).
///
/// Scope is deliberately narrow: only Elixir modules that changed, or sit one
/// module-edge hop from one that did, are ever parsed for functions -- see
/// [`target_module_ids`]. Within that scope, every extracted function
/// becomes a candidate and every resolvable call becomes an edge, but the
/// final index keeps only what [`filter_to_neighborhood`] calls "relevant":
/// changed functions, plus anything directly connected to one by a call
/// edge. This keeps the index small (the point of the drill-in feature)
/// without silently hiding a changed function's immediate callers/callees.
///
/// `FunctionInfo::start_line`/`end_line` are the raw 0-based tree-sitter rows
/// [`extract_functions`] reports; no line-number conversion happens here.
pub fn build_function_index(
    repo: &dyn GitRepo,
    graph: &ProjectGraph,
    base_oid: &str,
) -> Result<FunctionIndex> {
    let target_modules = target_module_ids(graph);
    if target_modules.is_empty() {
        return Ok(FunctionIndex::default());
    }

    let mut files: Vec<FileRef> = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    for id in &target_modules {
        let Some(node) = graph.node(id) else { continue };
        for file_ref in &node.files {
            if is_elixir_path(&file_ref.path) && seen_paths.insert(file_ref.path.clone()) {
                files.push(file_ref.clone());
            }
        }
    }

    let mut extracted: Vec<ExtractedModule> = Vec::new();
    for file_ref in &files {
        if file_ref.head_blob.is_none() {
            // Deleted file: no head content to extract from.
            continue;
        }
        let Some(content) = repo.head_content(&file_ref.path)? else {
            continue;
        };
        let changed_ranges = match load_file_diff(repo, base_oid, file_ref) {
            Ok(diff) => changed_head_ranges(&diff),
            Err(_) => Vec::new(),
        };

        for module_functions in extract_functions(&content) {
            let node_id = NodeId::from(format!("elixir:{}", module_functions.module));
            if !graph.nodes.contains_key(&node_id) {
                continue;
            }
            extracted.push(ExtractedModule {
                node_id,
                functions: module_functions.functions,
                calls: module_functions.calls,
                changed_ranges: changed_ranges.clone(),
            });
        }
    }

    let candidates = build_candidates(&extracted);
    let edges = build_edges(&extracted, &candidates, |name| {
        resolve_elixir_module(&graph.nodes, name)
    });
    Ok(filter_to_neighborhood(candidates, edges))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{DepEdge, DepKind, ModuleNode};
    use crate::pipeline::extract::elixir_extract::RawCallSite;

    fn info(
        module: &NodeId,
        name: &str,
        arity: usize,
        start: usize,
        end: usize,
        changed: bool,
    ) -> FunctionInfo {
        FunctionInfo {
            id: function_node_id(module, name, arity),
            name: name.to_string(),
            arity,
            start_line: start,
            end_line: end,
            public: true,
            changed,
        }
    }

    // -- range_is_changed ---------------------------------------------

    #[test]
    fn range_is_changed_true_when_overlapping() {
        assert!(range_is_changed((3, 8), &[(5, 5)]));
        assert!(range_is_changed((3, 8), &[(0, 3)]));
        assert!(range_is_changed((3, 8), &[(8, 20)]));
    }

    #[test]
    fn range_is_changed_false_when_disjoint() {
        assert!(!range_is_changed((3, 8), &[(9, 20)]));
        assert!(!range_is_changed((3, 8), &[(0, 2)]));
        assert!(!range_is_changed((3, 8), &[]));
    }

    // -- build_candidates -----------------------------------------------

    #[test]
    fn build_candidates_marks_functions_intersecting_changed_ranges() {
        let module = NodeId::from("elixir:MyApp.Accounts");
        let modules = vec![ExtractedModule {
            node_id: module.clone(),
            functions: vec![
                RawFunctionDef {
                    name: "create".to_string(),
                    arity: 1,
                    start_row: 1,
                    end_row: 3,
                    public: true,
                },
                RawFunctionDef {
                    name: "helper".to_string(),
                    arity: 0,
                    start_row: 10,
                    end_row: 12,
                    public: false,
                },
            ],
            calls: vec![],
            changed_ranges: vec![(2, 2)],
        }];

        let candidates = build_candidates(&modules);
        let infos = candidates.get(&module).expect("module present");
        let create = infos.iter().find(|f| f.name == "create").unwrap();
        assert!(create.changed);
        let helper = infos.iter().find(|f| f.name == "helper").unwrap();
        assert!(!helper.changed);
        assert!(!helper.public);
    }

    // -- resolve_in_candidates -------------------------------------------

    #[test]
    fn resolve_in_candidates_prefers_exact_arity_match() {
        let module = NodeId::from("elixir:M");
        let candidates = vec![
            info(&module, "f", 1, 0, 1, false),
            info(&module, "f", 2, 2, 3, false),
        ];
        let found = resolve_in_candidates("f", 2, &candidates).unwrap();
        assert_eq!(found.arity, 2);
    }

    #[test]
    fn resolve_in_candidates_falls_back_to_unique_name_match() {
        let module = NodeId::from("elixir:M");
        let candidates = vec![info(&module, "f", 1, 0, 1, false)];
        let found = resolve_in_candidates("f", 5, &candidates).unwrap();
        assert_eq!(found.arity, 1);
    }

    #[test]
    fn resolve_in_candidates_none_when_name_ambiguous_and_arity_mismatches() {
        let module = NodeId::from("elixir:M");
        let candidates = vec![
            info(&module, "f", 1, 0, 1, false),
            info(&module, "f", 3, 2, 3, false),
        ];
        assert!(resolve_in_candidates("f", 5, &candidates).is_none());
    }

    #[test]
    fn resolve_in_candidates_none_when_name_missing() {
        let module = NodeId::from("elixir:M");
        let candidates = vec![info(&module, "f", 1, 0, 1, false)];
        assert!(resolve_in_candidates("g", 1, &candidates).is_none());
    }

    // -- build_edges ------------------------------------------------------

    fn call(caller: (&str, usize), target: RawCallTarget) -> RawCallSite {
        RawCallSite {
            caller: (caller.0.to_string(), caller.1),
            target,
            row: 0,
        }
    }

    #[test]
    fn build_edges_resolves_local_call_within_same_module() {
        let module = NodeId::from("elixir:M");
        let modules = vec![ExtractedModule {
            node_id: module.clone(),
            functions: vec![
                RawFunctionDef {
                    name: "create".to_string(),
                    arity: 1,
                    start_row: 0,
                    end_row: 2,
                    public: true,
                },
                RawFunctionDef {
                    name: "helper".to_string(),
                    arity: 0,
                    start_row: 3,
                    end_row: 4,
                    public: false,
                },
            ],
            calls: vec![call(
                ("create", 1),
                RawCallTarget::Local {
                    name: "helper".to_string(),
                    arity: 0,
                },
            )],
            changed_ranges: vec![],
        }];
        let candidates = build_candidates(&modules);
        let edges = build_edges(&modules, &candidates, |_| None);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, function_node_id(&module, "create", 1));
        assert_eq!(edges[0].to, function_node_id(&module, "helper", 0));
    }

    #[test]
    fn build_edges_drops_call_when_caller_def_wasnt_extracted() {
        let module = NodeId::from("elixir:M");
        let modules = vec![ExtractedModule {
            node_id: module.clone(),
            functions: vec![RawFunctionDef {
                name: "helper".to_string(),
                arity: 0,
                start_row: 0,
                end_row: 1,
                public: false,
            }],
            calls: vec![call(
                ("ghost", 3),
                RawCallTarget::Local {
                    name: "helper".to_string(),
                    arity: 0,
                },
            )],
            changed_ranges: vec![],
        }];
        let candidates = build_candidates(&modules);
        let edges = build_edges(&modules, &candidates, |_| None);
        assert!(edges.is_empty());
    }

    #[test]
    fn build_edges_resolves_remote_call_to_function_precise_target() {
        let caller_module = NodeId::from("elixir:B");
        let callee_module = NodeId::from("elixir:A");
        let modules = vec![
            ExtractedModule {
                node_id: caller_module.clone(),
                functions: vec![RawFunctionDef {
                    name: "handle".to_string(),
                    arity: 1,
                    start_row: 0,
                    end_row: 2,
                    public: true,
                }],
                calls: vec![call(
                    ("handle", 1),
                    RawCallTarget::Remote {
                        module: "A".to_string(),
                        name: "create".to_string(),
                        arity: 1,
                    },
                )],
                changed_ranges: vec![],
            },
            ExtractedModule {
                node_id: callee_module.clone(),
                functions: vec![RawFunctionDef {
                    name: "create".to_string(),
                    arity: 1,
                    start_row: 0,
                    end_row: 2,
                    public: true,
                }],
                calls: vec![],
                changed_ranges: vec![],
            },
        ];
        let candidates = build_candidates(&modules);
        let edges = build_edges(&modules, &candidates, |name| {
            if name == "A" {
                Some(callee_module.clone())
            } else {
                None
            }
        });
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from, function_node_id(&caller_module, "handle", 1));
        assert_eq!(edges[0].to, function_node_id(&callee_module, "create", 1));
    }

    #[test]
    fn build_edges_falls_back_to_module_id_when_callee_not_function_precise() {
        let caller_module = NodeId::from("elixir:B");
        let callee_module = NodeId::from("elixir:A");
        let modules = vec![ExtractedModule {
            node_id: caller_module.clone(),
            functions: vec![RawFunctionDef {
                name: "handle".to_string(),
                arity: 1,
                start_row: 0,
                end_row: 2,
                public: true,
            }],
            calls: vec![call(
                ("handle", 1),
                RawCallTarget::Remote {
                    module: "A".to_string(),
                    name: "unknown_fn".to_string(),
                    arity: 9,
                },
            )],
            changed_ranges: vec![],
        }];
        // `A` has no extracted candidates at all (not in `candidates`).
        let candidates = build_candidates(&modules);
        let edges = build_edges(&modules, &candidates, |name| {
            if name == "A" {
                Some(callee_module.clone())
            } else {
                None
            }
        });
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to, callee_module);
    }

    #[test]
    fn build_edges_drops_call_when_remote_module_unresolvable() {
        let caller_module = NodeId::from("elixir:B");
        let modules = vec![ExtractedModule {
            node_id: caller_module.clone(),
            functions: vec![RawFunctionDef {
                name: "handle".to_string(),
                arity: 1,
                start_row: 0,
                end_row: 2,
                public: true,
            }],
            calls: vec![call(
                ("handle", 1),
                RawCallTarget::Remote {
                    module: "Nowhere".to_string(),
                    name: "f".to_string(),
                    arity: 0,
                },
            )],
            changed_ranges: vec![],
        }];
        let candidates = build_candidates(&modules);
        let edges = build_edges(&modules, &candidates, |_| None);
        assert!(edges.is_empty());
    }

    #[test]
    fn build_edges_dedupes_identical_from_to_pairs() {
        let module = NodeId::from("elixir:M");
        let modules = vec![ExtractedModule {
            node_id: module.clone(),
            functions: vec![
                RawFunctionDef {
                    name: "create".to_string(),
                    arity: 1,
                    start_row: 0,
                    end_row: 2,
                    public: true,
                },
                RawFunctionDef {
                    name: "helper".to_string(),
                    arity: 0,
                    start_row: 3,
                    end_row: 4,
                    public: false,
                },
            ],
            calls: vec![
                call(
                    ("create", 1),
                    RawCallTarget::Local {
                        name: "helper".to_string(),
                        arity: 0,
                    },
                ),
                call(
                    ("create", 1),
                    RawCallTarget::Local {
                        name: "helper".to_string(),
                        arity: 0,
                    },
                ),
            ],
            changed_ranges: vec![],
        }];
        let candidates = build_candidates(&modules);
        let edges = build_edges(&modules, &candidates, |_| None);
        assert_eq!(edges.len(), 1);
    }

    // -- filter_to_neighborhood -------------------------------------------

    #[test]
    fn filter_keeps_changed_functions_and_their_relevant_edge_endpoints() {
        let a = NodeId::from("elixir:A");
        let b = NodeId::from("elixir:B");
        let mut candidates = HashMap::new();
        candidates.insert(
            a.clone(),
            vec![
                info(&a, "create", 1, 0, 2, true),
                info(&a, "unrelated", 0, 5, 6, false),
            ],
        );
        candidates.insert(b.clone(), vec![info(&b, "handle", 1, 0, 2, false)]);

        let edges = vec![FunctionEdge {
            from: function_node_id(&b, "handle", 1),
            to: function_node_id(&a, "create", 1),
        }];

        let index = filter_to_neighborhood(candidates, edges);

        let a_rows = index.rows_for(&a).expect("A kept");
        assert_eq!(a_rows.len(), 1, "unrelated unchanged fn dropped");
        assert_eq!(a_rows[0].name, "create");

        let b_rows = index.rows_for(&b).expect("B kept via relevant edge");
        assert_eq!(b_rows.len(), 1);
        assert_eq!(b_rows[0].name, "handle");
        assert!(!b_rows[0].changed);

        assert_eq!(index.edges.len(), 1);
    }

    #[test]
    fn filter_drops_edge_and_module_signature() {
        let a = NodeId::from("elixir:A");
        let b = NodeId::from("elixir:B");
        let mut candidates = HashMap::new();
        candidates.insert(a.clone(), vec![info(&a, "create", 1, 0, 2, false)]);
        candidates.insert(b.clone(), vec![info(&b, "handle", 1, 0, 2, false)]);

        // Neither endpoint changed -- not relevant.
        let edges = vec![FunctionEdge {
            from: function_node_id(&b, "handle", 1),
            to: function_node_id(&a, "create", 1),
        }];

        let index = filter_to_neighborhood(candidates, edges);
        assert!(index.is_empty());
    }

    #[test]
    fn filter_keeps_module_level_edge_when_caller_changed() {
        let a = NodeId::from("elixir:A");
        let module_target = NodeId::from("elixir:External");
        let mut candidates = HashMap::new();
        candidates.insert(a.clone(), vec![info(&a, "create", 1, 0, 2, true)]);

        let edges = vec![FunctionEdge {
            from: function_node_id(&a, "create", 1),
            to: module_target.clone(),
        }];

        let index = filter_to_neighborhood(candidates, edges);
        assert_eq!(index.edges.len(), 1);
        assert_eq!(index.edges[0].to, module_target);
    }

    // -- target_module_ids --------------------------------------------------

    fn module_node(id: &str, status: GitStatus, files: Vec<FileRef>) -> ModuleNode {
        ModuleNode {
            id: NodeId::from(id),
            display_name: id.to_string(),
            parent: None,
            children: vec![],
            status,
            files,
        }
    }

    fn one_file(path: &str) -> Vec<FileRef> {
        vec![FileRef {
            path: PathBuf::from(path),
            base_blob: Some("b".to_string()),
            head_blob: Some("h".to_string()),
        }]
    }

    #[test]
    fn target_module_ids_includes_changed_and_one_hop_neighbors() {
        let a = module_node("elixir:A", GitStatus::Modified, one_file("a.ex"));
        let b = module_node("elixir:B", GitStatus::Unchanged, one_file("b.ex"));
        let c = module_node("elixir:C", GitStatus::Unchanged, one_file("c.ex"));
        let mut nodes = HashMap::new();
        nodes.insert(a.id.clone(), a.clone());
        nodes.insert(b.id.clone(), b.clone());
        nodes.insert(c.id.clone(), c.clone());

        let graph = ProjectGraph {
            nodes,
            roots: vec![],
            edges: vec![DepEdge {
                from: b.id.clone(),
                to: a.id.clone(),
                kind: DepKind::RemoteCall,
            }],
        };

        let targets = target_module_ids(&graph);
        assert!(targets.contains(&a.id));
        assert!(
            targets.contains(&b.id),
            "one-hop neighbor via edge included"
        );
        assert!(!targets.contains(&c.id), "two hops away, not included");
    }

    #[test]
    fn target_module_ids_excludes_modules_with_no_files() {
        let a = module_node("elixir:A", GitStatus::Modified, vec![]);
        let mut nodes = HashMap::new();
        nodes.insert(a.id.clone(), a.clone());
        let graph = ProjectGraph {
            nodes,
            roots: vec![],
            edges: vec![],
        };
        assert!(target_module_ids(&graph).is_empty());
    }
}
