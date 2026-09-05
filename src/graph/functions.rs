//! Sidecar function-level data for the drill-in feature.
//!
//! This is deliberately NOT part of [`crate::graph::model::ProjectGraph`]:
//! the module graph stays exactly as it is today (nodes/edges keyed by
//! module [`NodeId`]), and function-level detail lives alongside it in a
//! [`FunctionIndex`], keyed by the id of the module that OWNS each function.
//! Callers that don't care about function-level drill-in never have to
//! touch this module at all; callers that do look the module up in
//! [`FunctionIndex::rows_for`] on demand.
//!
//! ## Id scheme
//!
//! A function's [`NodeId`] is derived from its owning module's id, its name,
//! and its arity: `<module>#<name>/<arity>`, e.g.
//! `elixir:MyApp.Accounts#create/2`. The `#` separator can't appear in a
//! module id (module ids are dotted/`::`-qualified names, never containing
//! `#`), so splitting on the first `#` unambiguously recovers the owning
//! module id from a function id -- see [`function_owner`] and
//! [`is_function_id`]. [`function_node_id`] is the inverse: build a function
//! id from its parts.
//!
//! ## Static calls only
//!
//! [`FunctionEdge`] represents a statically resolved call: `from` is always
//! a function id (the caller). `to` is a function id only when the callee
//! resolved function-precise (same-module local call, or a remote call whose
//! target module and function were both known at extraction time); when only
//! the target module is known -- the callee couldn't be pinned to a specific
//! function/arity, e.g. a dynamic dispatch or an unresolved remote call --
//! `to` falls back to that module's id. Nothing here executes code or
//! reasons about runtime dispatch (behaviours, protocols, `apply/3`, ...);
//! this is a best-effort static approximation, same spirit as the existing
//! module-level dependency edges.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::graph::model::NodeId;

/// One function/method found in a module, with enough position/visibility
/// data to drive drill-in UI and change highlighting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionInfo {
    /// This function's stable id (see the module doc's id scheme).
    pub id: NodeId,
    /// The function's bare name (no module qualification, no arity suffix).
    pub name: String,
    /// Number of parameters in the function's head.
    pub arity: usize,
    /// 0-based, inclusive: the first head-file line of the function's
    /// definition (including its head/guard).
    pub start_line: usize,
    /// 0-based, inclusive: the last head-file line of the function's
    /// definition (including its body).
    pub end_line: usize,
    /// Whether the function is part of the module's public interface (e.g.
    /// Elixir `def`/`defmacro` vs. `defp`/`defmacrop`).
    pub public: bool,
    /// Whether this function's lines were touched by the diff.
    pub changed: bool,
}

/// A directed static call edge between two functions (or from a function to
/// a module, when the callee couldn't be resolved function-precise -- see
/// the module doc's "static calls only" section).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionEdge {
    /// The calling function's id.
    pub from: NodeId,
    /// The callee: a function id when resolved function-precise, otherwise
    /// the callee's owning module id.
    pub to: NodeId,
}

/// Sidecar function-level index: every extracted [`FunctionInfo`], grouped
/// by the id of the module that owns it, plus every resolved
/// [`FunctionEdge`]. See the module doc for why this lives outside
/// [`crate::graph::model::ProjectGraph`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionIndex {
    /// Functions, keyed by owning module id.
    pub functions: HashMap<NodeId, Vec<FunctionInfo>>,
    /// Every resolved call edge.
    pub edges: Vec<FunctionEdge>,
}

impl FunctionIndex {
    /// True if this index has no functions and no edges at all.
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty() && self.edges.is_empty()
    }

    /// The functions owned by module `id`, if any were extracted for it.
    pub fn rows_for(&self, id: &NodeId) -> Option<&[FunctionInfo]> {
        self.functions.get(id).map(|rows| rows.as_slice())
    }
}

/// Build a function's [`NodeId`] from its owning module id, name, and arity:
/// `<module>#<name>/<arity>`.
pub fn function_node_id(module: &NodeId, name: &str, arity: usize) -> NodeId {
    NodeId::from(format!("{module}#{name}/{arity}"))
}

/// Recover the owning module id from a function id (the text before the
/// first `#`), or `None` if `id` has no `#` (i.e. isn't a function id).
pub fn function_owner(id: &NodeId) -> Option<NodeId> {
    let text = id.to_string();
    text.split_once('#').map(|(module, _)| NodeId::from(module))
}

/// True if `id` looks like a function id (contains a `#`), false for a
/// plain module id.
pub fn is_function_id(id: &NodeId) -> bool {
    id.to_string().contains('#')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_node_id_round_trips_through_function_owner() {
        let module = NodeId::from("elixir:MyApp.Accounts");
        let fn_id = function_node_id(&module, "create", 2);
        assert_eq!(fn_id, NodeId::from("elixir:MyApp.Accounts#create/2"));
        assert_eq!(function_owner(&fn_id), Some(module));
    }

    #[test]
    fn function_owner_is_none_for_a_plain_module_id() {
        assert_eq!(function_owner(&NodeId::from("elixir:MyApp.Accounts")), None);
    }

    #[test]
    fn is_function_id_distinguishes_function_and_module_ids() {
        let module = NodeId::from("elixir:MyApp.Accounts");
        let fn_id = function_node_id(&module, "create", 2);
        assert!(is_function_id(&fn_id));
        assert!(!is_function_id(&module));
    }

    #[test]
    fn function_index_is_empty_and_rows_for() {
        let mut index = FunctionIndex::default();
        assert!(index.is_empty());

        let module = NodeId::from("elixir:MyApp.Accounts");
        let info = FunctionInfo {
            id: function_node_id(&module, "create", 2),
            name: "create".to_string(),
            arity: 2,
            start_line: 3,
            end_line: 8,
            public: true,
            changed: false,
        };
        index.functions.insert(module.clone(), vec![info.clone()]);
        assert!(!index.is_empty());
        assert_eq!(index.rows_for(&module), Some(&[info][..]));
        assert_eq!(index.rows_for(&NodeId::from("missing")), None);
    }

    #[test]
    fn function_index_round_trips_through_json() {
        let module = NodeId::from("elixir:MyApp.Accounts");
        let callee = NodeId::from("elixir:MyApp.Repo");
        let mut index = FunctionIndex::default();
        index.functions.insert(
            module.clone(),
            vec![FunctionInfo {
                id: function_node_id(&module, "create", 2),
                name: "create".to_string(),
                arity: 2,
                start_line: 3,
                end_line: 8,
                public: true,
                changed: true,
            }],
        );
        index.edges.push(FunctionEdge {
            from: function_node_id(&module, "create", 2),
            to: callee,
        });

        let json = serde_json::to_string(&index).expect("serialize");
        let round_tripped: FunctionIndex = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, index);
    }
}
