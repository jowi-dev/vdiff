//! [`ElixirExtract`]: a [`ModuleExtractor`] over `tree-sitter-elixir`.
//!
//! Elixir has no dedicated `defmodule` syntax at the parser level --
//! `defmodule`, `alias`, `import`, `use`, and `require` all parse as
//! ordinary `call` nodes (`target: (identifier)`, `arguments`, optional
//! `do_block`). This walks the tree looking for those five call shapes:
//! `defmodule` pushes a new [`ModuleDef`] (qualified by dotted
//! concatenation with whatever `defmodule` currently encloses it, per
//! Elixir's nested-module semantics) and recurses into its `do_block`;
//! the other four attribute a [`DepRef`] to the innermost enclosing module.
//! Directives outside any `defmodule` (script-style top-level code) are
//! dropped -- they have no module to attach to.
//!
//! Two more shapes attribute a [`DepRef`] without needing any directive at
//! all, since dotted module names lex as a single `alias` token in
//! tree-sitter-elixir: a fully qualified remote call (`App.Leads.create_lead(...)`
//! parses as `call target: (dot left: (alias) right: (identifier))`, so
//! `dot.left`'s text is already the full target module name) and a struct
//! literal (`%App.Leads.Lead{}` parses as `(map (struct (alias)))`). Both
//! are attributed as [`DepKind::RemoteCall`]. A dot-call whose left side is
//! not an `alias` node -- a variable (`foo.bar()`) or an atom
//! (`:erlang.node()`) -- is not a module reference and is skipped;
//! erlang-atom-module calls are out of scope for this extractor.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::graph::model::DepKind;
use crate::pipeline::extract::{DepRef, ModuleDef, ModuleExtractor};

/// Extracts Elixir modules and alias/import/use/require dependencies via
/// `tree-sitter-elixir`.
pub struct ElixirExtract;

impl ModuleExtractor for ElixirExtract {
    fn extract(&self, _path: &Path, source: &str) -> Vec<ModuleDef> {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_elixir::LANGUAGE.into())
            .is_err()
        {
            return Vec::new();
        }
        let Some(tree) = parser.parse(source, None) else {
            return Vec::new();
        };
        let mut ctx = Ctx {
            src: source.as_bytes(),
            defs: Vec::new(),
            stack: Vec::new(),
        };
        walk(&mut ctx, tree.root_node());
        ctx.defs
    }
}

/// Walk state: the module defs accumulated so far, and a stack of indices
/// into `defs` naming the `defmodule` nesting currently enclosing the node
/// being visited (innermost last).
struct Ctx<'a> {
    src: &'a [u8],
    defs: Vec<ModuleDef>,
    stack: Vec<usize>,
}

fn walk(ctx: &mut Ctx, node: Node) {
    if node.kind() == "call" {
        if let Some(target) = node.child_by_field_name("target") {
            match target.kind() {
                "identifier" => {
                    let name = node_text(ctx.src, target);
                    match name.as_str() {
                        "defmodule" => return handle_defmodule(ctx, node),
                        "alias" | "import" | "use" | "require" => {
                            return handle_directive(ctx, node, &name)
                        }
                        _ => {}
                    }
                }
                "dot" => handle_remote_call(ctx, target),
                _ => {}
            }
        }
    } else if node.kind() == "struct" {
        handle_struct_literal(ctx, node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(ctx, child);
    }
}

/// Handle a `defmodule Name do ... end` call: push a new [`ModuleDef`]
/// qualified against the enclosing module (if any), then recurse into its
/// `do_block` under that new context.
fn handle_defmodule(ctx: &mut Ctx, node: Node) {
    let Some(alias_node) = first_call_argument(node) else {
        return;
    };
    if alias_node.kind() != "alias" {
        return;
    }
    let segment = node_text(ctx.src, alias_node);
    let full_name = qualify(ctx, &segment);
    ctx.defs.push(ModuleDef {
        name: full_name,
        dep_refs: Vec::new(),
    });
    ctx.stack.push(ctx.defs.len() - 1);
    if let Some(body) = child_of_kind(node, "do_block") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            walk(ctx, child);
        }
    }
    ctx.stack.pop();
}

/// Dotted-concatenate `segment` onto the currently enclosing module's name,
/// per Elixir's nested `defmodule` semantics (`defmodule Bar` inside
/// `defmodule Foo` is `Foo.Bar`, regardless of dots already in `segment`).
fn qualify(ctx: &Ctx, segment: &str) -> String {
    match ctx.stack.last() {
        Some(&idx) => format!("{}.{segment}", ctx.defs[idx].name),
        None => segment.to_string(),
    }
}

/// Handle an `alias`/`import`/`use`/`require` call: attribute a [`DepRef`]
/// per resolved target name to the innermost enclosing module. A no-op if
/// there is no enclosing module (top-level script code).
fn handle_directive(ctx: &mut Ctx, node: Node, directive: &str) {
    let Some(&current_idx) = ctx.stack.last() else {
        return;
    };
    let kind = match directive {
        "alias" => DepKind::Alias,
        "import" => DepKind::Import,
        "use" => DepKind::Use,
        "require" => DepKind::Require,
        _ => return,
    };
    let Some(arg) = first_call_argument(node) else {
        return;
    };
    for name in directive_target_names(ctx.src, arg) {
        ctx.defs[current_idx].dep_refs.push(DepRef { name, kind });
    }
}

/// Handle a `call` node's `dot` target (`App.Leads.create_lead(...)`):
/// attribute a [`DepKind::RemoteCall`] [`DepRef`] to the innermost enclosing
/// module if the dot's left side is a module alias, i.e. skip dot-calls on
/// a variable (`foo.bar()`) or an atom (`:erlang.node()`). A no-op if there
/// is no enclosing module.
fn handle_remote_call(ctx: &mut Ctx, dot: Node) {
    let Some(&current_idx) = ctx.stack.last() else {
        return;
    };
    let Some(left) = dot.child_by_field_name("left") else {
        return;
    };
    if left.kind() != "alias" {
        return;
    }
    let name = node_text(ctx.src, left);
    push_dep_ref(ctx, current_idx, name, DepKind::RemoteCall);
}

/// Handle a `%App.Leads.Lead{}` struct literal (parses as `(struct
/// (alias))`): attribute a [`DepKind::RemoteCall`] [`DepRef`] to the
/// innermost enclosing module. A no-op for update syntax on a variable
/// (`%struct_var{}`, whose child is not an `alias`) or if there is no
/// enclosing module.
fn handle_struct_literal(ctx: &mut Ctx, node: Node) {
    let Some(&current_idx) = ctx.stack.last() else {
        return;
    };
    let Some(alias_node) = child_of_kind(node, "alias") else {
        return;
    };
    let name = node_text(ctx.src, alias_node);
    push_dep_ref(ctx, current_idx, name, DepKind::RemoteCall);
}

/// Push a [`DepRef`] onto `defs[idx].dep_refs`, unless one with the same
/// name and kind is already present -- so repeated remote calls/struct
/// literals targeting the same module within one file produce a single
/// dep ref (and therefore a single edge once resolved).
fn push_dep_ref(ctx: &mut Ctx, idx: usize, name: String, kind: DepKind) {
    let refs = &mut ctx.defs[idx].dep_refs;
    if !refs.iter().any(|d| d.name == name && d.kind == kind) {
        refs.push(DepRef { name, kind });
    }
}

/// Resolve the module name(s) an `alias`/`import`/`use`/`require`
/// directive's first argument refers to: a plain alias (`MyApp.Repo`), or a
/// multi-alias group (`MyApp.Accounts.{User, Profile}`, which parses as a
/// `dot` node whose `right` side is a `tuple` of bare alias segments).
fn directive_target_names(src: &[u8], arg: Node) -> Vec<String> {
    match arg.kind() {
        "alias" => vec![node_text(src, arg)],
        "dot" => {
            let (Some(left), Some(right)) = (
                arg.child_by_field_name("left"),
                arg.child_by_field_name("right"),
            ) else {
                return Vec::new();
            };
            let base = node_text(src, left);
            match right.kind() {
                "tuple" => {
                    let mut cursor = right.walk();
                    right
                        .named_children(&mut cursor)
                        .filter(|n| n.kind() == "alias")
                        .map(|n| format!("{base}.{}", node_text(src, n)))
                        .collect()
                }
                "alias" => vec![format!("{base}.{}", node_text(src, right))],
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// A `call` node's first (named) argument, if it has an `arguments` child
/// at all.
fn first_call_argument(call: Node) -> Option<Node> {
    let args = child_of_kind(call, "arguments")?;
    let mut cursor = args.walk();
    let first = args.named_children(&mut cursor).next();
    first
}

fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

fn node_text(src: &[u8], node: Node) -> String {
    node.utf8_text(src).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(source: &str) -> Vec<ModuleDef> {
        ElixirExtract.extract(Path::new("lib/my_app.ex"), source)
    }

    fn dep(name: &str, kind: DepKind) -> DepRef {
        DepRef {
            name: name.to_string(),
            kind,
        }
    }

    #[test]
    fn two_top_level_modules() {
        let defs = extract(
            r#"
            defmodule MyApp.Foo do
            end

            defmodule MyApp.Bar do
            end
            "#,
        );
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["MyApp.Foo", "MyApp.Bar"]);
    }

    #[test]
    fn nested_defmodule_qualifies_by_dotted_concatenation() {
        let defs = extract(
            r#"
            defmodule Foo do
                defmodule Bar do
                end
            end
            "#,
        );
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["Foo", "Foo.Bar"]);
    }

    #[test]
    fn multi_alias_group_expands_to_one_ref_per_segment() {
        let defs = extract(
            r#"
            defmodule MyApp.Accounts do
                alias MyApp.Accounts.{User, Profile}
            end
            "#,
        );
        assert_eq!(defs.len(), 1);
        assert_eq!(
            defs[0].dep_refs,
            vec![
                dep("MyApp.Accounts.User", DepKind::Alias),
                dep("MyApp.Accounts.Profile", DepKind::Alias),
            ]
        );
    }

    #[test]
    fn use_import_require_produce_proper_dep_kinds() {
        let defs = extract(
            r#"
            defmodule MyApp.Accounts do
                alias MyApp.Repo
                import Ecto.Query
                use MyApp.Schema
                require Logger
            end
            "#,
        );
        assert_eq!(defs.len(), 1);
        assert_eq!(
            defs[0].dep_refs,
            vec![
                dep("MyApp.Repo", DepKind::Alias),
                dep("Ecto.Query", DepKind::Import),
                dep("MyApp.Schema", DepKind::Use),
                dep("Logger", DepKind::Require),
            ]
        );
    }

    #[test]
    fn directives_attach_to_innermost_enclosing_module() {
        let defs = extract(
            r#"
            defmodule Outer do
                alias Outer.Thing

                defmodule Inner do
                    alias Inner.Thing
                end
            end
            "#,
        );
        let outer = defs.iter().find(|d| d.name == "Outer").unwrap();
        assert_eq!(outer.dep_refs, vec![dep("Outer.Thing", DepKind::Alias)]);
        let inner = defs.iter().find(|d| d.name == "Outer.Inner").unwrap();
        assert_eq!(inner.dep_refs, vec![dep("Inner.Thing", DepKind::Alias)]);
    }

    #[test]
    fn directive_outside_any_module_is_dropped() {
        let defs = extract("alias Foo.Bar\n");
        assert!(defs.is_empty());
    }

    #[test]
    fn alias_as_still_yields_one_ref_to_the_aliased_target() {
        let defs = extract(
            r#"
            defmodule MyApp.Accounts do
                alias MyApp.Repo, as: R
            end
            "#,
        );
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].dep_refs, vec![dep("MyApp.Repo", DepKind::Alias)]);
    }

    #[test]
    fn fully_qualified_remote_call_with_no_alias_yields_a_dep_ref() {
        let defs = extract(
            r#"
            defmodule MyApp.Accounts do
                def create(attrs) do
                    App.Leads.create_lead(attrs)
                end
            end
            "#,
        );
        assert_eq!(defs.len(), 1);
        assert_eq!(
            defs[0].dep_refs,
            vec![dep("App.Leads", DepKind::RemoteCall)]
        );
    }

    #[test]
    fn struct_literal_yields_a_dep_ref_to_its_module() {
        let defs = extract(
            r#"
            defmodule MyApp.Accounts do
                def build do
                    %App.Leads.Lead{}
                end
            end
            "#,
        );
        assert_eq!(defs.len(), 1);
        assert_eq!(
            defs[0].dep_refs,
            vec![dep("App.Leads.Lead", DepKind::RemoteCall)]
        );
    }

    #[test]
    fn repeated_remote_calls_to_the_same_module_dedupe_to_one_ref() {
        let defs = extract(
            r#"
            defmodule MyApp.Accounts do
                def create(attrs) do
                    App.Leads.create_lead(attrs)
                end

                def update(attrs) do
                    App.Leads.update_lead(attrs)
                end
            end
            "#,
        );
        assert_eq!(defs.len(), 1);
        assert_eq!(
            defs[0].dep_refs,
            vec![dep("App.Leads", DepKind::RemoteCall)]
        );
    }

    #[test]
    fn dot_call_on_variable_or_atom_is_not_a_module_reference() {
        let defs = extract(
            r#"
            defmodule MyApp.Accounts do
                def check(foo) do
                    foo.bar()
                    :erlang.node()
                end
            end
            "#,
        );
        assert_eq!(defs.len(), 1);
        assert!(defs[0].dep_refs.is_empty());
    }

    #[test]
    fn unparseable_source_yields_no_defs() {
        // tree-sitter is error-tolerant, so this mostly documents that
        // extract() never panics on garbage input rather than that it
        // necessarily returns nothing.
        let defs = extract("!!! not elixir {{{");
        assert!(defs.is_empty());
    }
}
