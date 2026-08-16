//! [`ModuleExtractor`]: the trait per-language extractors implement to turn
//! source text into module definitions and their dependency references.
//! Resolving [`DepRef`]s into [`crate::graph::model::DepEdge`]s against the
//! whole-project name table is [`crate::pipeline::resolve`]'s job, not
//! this one -- extractors only see one file at a time.

pub mod rust_extract;

use std::path::Path;

use crate::graph::model::DepKind;

/// A named dependency an extracted module refers to (a Rust `use` path, an
/// Elixir `alias`/`import`/`require`/`use` target, ...), not yet resolved
/// to a [`crate::graph::model::NodeId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepRef {
    /// The full referenced path/name, e.g. `"crate::pipeline::repo::GitRepo"`
    /// or `"serde::Serialize"`.
    pub name: String,
    /// How the dependency was declared.
    pub kind: DepKind,
}

/// One module found in a source file, relative to the file's own location
/// (crate-/directory-qualification happens in [`crate::pipeline::resolve`]).
/// The empty string names the file's own top-level module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDef {
    /// This module's name, relative to the file: `""` for the file-level
    /// module itself, `"foo"` / `"foo::bar"` for inline nested modules.
    pub name: String,
    /// Dependencies this module declares.
    pub dep_refs: Vec<DepRef>,
}

/// Extracts [`ModuleDef`]s from one file's source text.
pub trait ModuleExtractor {
    /// Parse `source` (the content of the file at `path`) into its module
    /// definitions. Returns an empty vec if `source` doesn't parse.
    fn extract(&self, path: &Path, source: &str) -> Vec<ModuleDef>;
}
