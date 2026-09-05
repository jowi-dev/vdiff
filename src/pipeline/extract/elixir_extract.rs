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

/// One module's extracted function definitions and the call sites made from
/// within them, as raw (unresolved-to-graph-ids) data -- pairing with
/// [`crate::graph::functions`] happens one layer up, in
/// [`crate::pipeline::resolve`].
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleFunctions {
    /// This module's fully qualified dotted name (matches
    /// [`ModuleDef::name`] qualification, via the same [`qualify`]).
    pub module: String,
    /// Every function/macro def (`def`/`defp`/`defmacro`/`defmacrop`) found
    /// directly in this module, deduped across clauses of the same
    /// name/arity.
    pub functions: Vec<RawFunctionDef>,
    /// Every call site made from within one of `functions`' bodies.
    pub calls: Vec<RawCallSite>,
}

/// One `def`/`defp`/`defmacro`/`defmacrop`, deduped across clauses: a
/// multi-clause function (`def foo(1) do ... end` / `def foo(x) do ... end`)
/// yields a single entry spanning its first clause's start through its last
/// clause's end.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFunctionDef {
    /// The function's bare name (no module qualification).
    pub name: String,
    /// Parameter count in the head (a default-arg parameter, `x \\ 1`,
    /// counts as one).
    pub arity: usize,
    /// 0-based, inclusive tree-sitter row of the earliest clause's start
    /// (head, including any `when` guard).
    pub start_row: usize,
    /// 0-based, inclusive tree-sitter row of the latest clause's end
    /// (including its body).
    pub end_row: usize,
    /// `true` for `def`/`defmacro`, `false` for `defp`/`defmacrop`.
    pub public: bool,
}

/// A call made from within a function body.
#[derive(Debug, Clone, PartialEq)]
pub struct RawCallSite {
    /// The enclosing function's name and arity (the def the call was found
    /// inside). Calls outside any function body are never collected.
    pub caller: (String, usize),
    /// The resolved (or best-effort) call target.
    pub target: RawCallTarget,
    /// 0-based tree-sitter row the call appears on.
    pub row: usize,
}

/// A call's target, resolved as far as static analysis of a single file
/// allows.
#[derive(Debug, Clone, PartialEq)]
pub enum RawCallTarget {
    /// A bare identifier call (`helper(x)`) resolved to a def in the same
    /// module.
    Local {
        /// Callee name.
        name: String,
        /// Effective arg count (pipe-adjusted; see [`extract_functions`]).
        arity: usize,
    },
    /// A dotted call (`Repo.insert(x)`), with `module` resolved through the
    /// enclosing module's `alias` directives where possible.
    Remote {
        /// Resolved (or, failing resolution, as-written) target module name.
        module: String,
        /// Callee name.
        name: String,
        /// Effective arg count (pipe-adjusted; see [`extract_functions`]).
        arity: usize,
    },
}

/// Extract every module's function defs and static call sites from one
/// Elixir source file.
///
/// Resolution rules (v1, deliberately simple):
/// - `alias` directives are collected module-wide (anywhere in the module
///   body, applied to the whole module), including `Mod.{A, B}` groups and
///   `as:` renames -- unlike [`ElixirExtract::extract`]'s `dep_refs`, which
///   only need the alias *target*, here the *rename* itself matters because
///   it's the name used at call sites.
/// - A remote call `X.fun(...)`'s first dotted segment is looked up in the
///   alias map; an unresolved segment is assumed already fully qualified.
/// - A bare call `helper(...)` is only treated as a same-module call once a
///   `def`/`defp`/`defmacro`/`defmacrop` named `helper` is known to exist in
///   the same module -- otherwise it's almost certainly a special form
///   (`if`, `case`, `raise`, ...) or a call into an implicit import, neither
///   of which this extractor tries to resolve.
/// - `x |> f(y)` bumps `f`'s effective arity by one for the piped-in value;
///   chained pipes (`x |> f() |> g()`) apply the same bump at each stage.
/// - captures (`&Repo.insert/1`, `&helper/2`) are not extracted: the shape
///   is a `unary_operator` wrapping a `/`-shaped `binary_operator`, cheap to
///   spot but not obviously worth the extra branch for v1's static-calls-only
///   scope; revisit if drill-in gaps show up in practice.
pub fn extract_functions(source: &str) -> Vec<ModuleFunctions> {
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

    let src = source.as_bytes();
    let mut scratches: Vec<ModuleScratch> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    collect_modules(src, tree.root_node(), &mut stack, &mut scratches);

    scratches
        .into_iter()
        .map(|scratch| build_module_functions(src, scratch))
        .collect()
}

/// A module's raw defs/aliases, collected before any call-site resolution
/// (so `alias`es declared after a `def` still apply to calls inside it).
struct ModuleScratch<'a> {
    name: String,
    /// `(alias_name, resolved_target)` pairs, in declaration order.
    aliases: Vec<(String, String)>,
    raw_defs: Vec<RawDefEntry<'a>>,
}

/// One `def`/`defp`/`defmacro`/`defmacrop` call node, not yet parsed into a
/// [`RawFunctionDef`] (deferred until all of the module's aliases are known).
struct RawDefEntry<'a> {
    keyword: &'static str,
    def_node: Node<'a>,
}

/// Phase 1: walk the whole tree once, discovering every `defmodule` (pushing
/// a fresh [`ModuleScratch`] and qualifying its name the same way
/// [`handle_defmodule`] does), and attributing every `def`-family call and
/// `alias` directive to the innermost enclosing scratch. Nested `defmodule`s
/// get their own scratch and are not walked as part of their parent's.
fn collect_modules<'a>(
    src: &[u8],
    node: Node<'a>,
    stack: &mut Vec<usize>,
    scratches: &mut Vec<ModuleScratch<'a>>,
) {
    if node.kind() == "call" {
        if let Some(target) = node.child_by_field_name("target") {
            if target.kind() == "identifier" {
                let name = node_text(src, target);
                match name.as_str() {
                    "defmodule" => {
                        return collect_defmodule(src, node, stack, scratches);
                    }
                    "def" | "defp" | "defmacro" | "defmacrop" => {
                        if let Some(&idx) = stack.last() {
                            let keyword = match name.as_str() {
                                "def" => "def",
                                "defp" => "defp",
                                "defmacro" => "defmacro",
                                _ => "defmacrop",
                            };
                            scratches[idx].raw_defs.push(RawDefEntry {
                                keyword,
                                def_node: node,
                            });
                        }
                        return;
                    }
                    "alias" => {
                        if let Some(&idx) = stack.last() {
                            if let Some(arg) = first_call_argument(node) {
                                collect_alias_pairs(src, node, arg, &mut scratches[idx].aliases);
                            }
                        }
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_modules(src, child, stack, scratches);
    }
}

fn collect_defmodule<'a>(
    src: &[u8],
    node: Node<'a>,
    stack: &mut Vec<usize>,
    scratches: &mut Vec<ModuleScratch<'a>>,
) {
    let Some(alias_node) = first_call_argument(node) else {
        return;
    };
    if alias_node.kind() != "alias" {
        return;
    }
    let segment = node_text(src, alias_node);
    let full_name = match stack.last() {
        Some(&idx) => format!("{}.{segment}", scratches[idx].name),
        None => segment,
    };
    scratches.push(ModuleScratch {
        name: full_name,
        aliases: Vec::new(),
        raw_defs: Vec::new(),
    });
    stack.push(scratches.len() - 1);
    if let Some(body) = child_of_kind(node, "do_block") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            collect_modules(src, child, stack, scratches);
        }
    }
    stack.pop();
}

/// Resolve an `alias` directive's argument into `(alias_name, target)`
/// pairs, preserving `as:` renames (unlike [`directive_target_names`], which
/// only needs the target and discards the rename):
/// - `alias MyApp.Repo` -> `("Repo", "MyApp.Repo")`
/// - `alias MyApp.Repo, as: R` -> `("R", "MyApp.Repo")`
/// - `alias MyApp.Accounts.{User, Profile}` -> `("User", "MyApp.Accounts.User")`,
///   `("Profile", "MyApp.Accounts.Profile")`
fn collect_alias_pairs(src: &[u8], call_node: Node, arg: Node, out: &mut Vec<(String, String)>) {
    match arg.kind() {
        "alias" => {
            let target = node_text(src, arg);
            let alias_name =
                as_rename(src, call_node).unwrap_or_else(|| last_segment(&target).to_string());
            out.push((alias_name, target));
        }
        "dot" => {
            let (Some(left), Some(right)) = (
                arg.child_by_field_name("left"),
                arg.child_by_field_name("right"),
            ) else {
                return;
            };
            let base = node_text(src, left);
            match right.kind() {
                "tuple" => {
                    let mut cursor = right.walk();
                    for segment in right
                        .named_children(&mut cursor)
                        .filter(|n| n.kind() == "alias")
                    {
                        let seg_text = node_text(src, segment);
                        let target = format!("{base}.{seg_text}");
                        out.push((seg_text, target));
                    }
                }
                "alias" => {
                    let seg_text = node_text(src, right);
                    let target = format!("{base}.{seg_text}");
                    out.push((seg_text, target));
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// The last dotted segment of a module name (`"MyApp.Accounts.Repo"` ->
/// `"Repo"`), used as the default alias name a plain `alias Mod` directive
/// introduces.
fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// If `alias`'s argument list carries a trailing `as: Rename` keyword pair,
/// return the rename text (`"R"`). `alias`'s `arguments` node is `(alias,
/// keywords(pair key: value:))` for `as:` and `(alias)` otherwise.
fn as_rename(src: &[u8], node: Node) -> Option<String> {
    let args = child_of_kind(node, "arguments")?;
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        if child.kind() == "keywords" {
            let mut pcursor = child.walk();
            for pair in child.named_children(&mut pcursor) {
                if pair.kind() != "pair" {
                    continue;
                }
                let key = pair.child_by_field_name("key")?;
                let key_text = node_text(src, key);
                if key_text.trim().trim_end_matches(':') == "as" {
                    let value = pair.child_by_field_name("value")?;
                    return Some(node_text(src, value));
                }
            }
        }
    }
    None
}

/// Phase 2: turn a fully-populated [`ModuleScratch`] into a
/// [`ModuleFunctions`] -- dedupe its raw def clauses, then walk each
/// deduped function's body for call sites, now that the module's complete
/// alias map and function-name set are both known.
fn build_module_functions(src: &[u8], scratch: ModuleScratch) -> ModuleFunctions {
    let functions = dedupe_defs(src, &scratch.raw_defs);
    let known_names: std::collections::HashSet<&str> =
        functions.iter().map(|f| f.name.as_str()).collect();

    let mut calls = Vec::new();
    for entry in &scratch.raw_defs {
        let Some((name, arity)) = parse_def_head(src, entry.def_node) else {
            continue;
        };
        let Some(body) = child_of_kind(entry.def_node, "do_block") else {
            continue;
        };
        let caller = (name, arity);
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            walk_calls(
                src,
                child,
                &caller,
                &scratch.aliases,
                &known_names,
                &mut calls,
            );
        }
    }

    ModuleFunctions {
        module: scratch.name,
        functions,
        calls,
    }
}

/// Dedupe raw `def`/`defp`/`defmacro`/`defmacrop` clauses by `(name,
/// arity)`, keeping the earliest `start_row` and latest `end_row` across
/// clauses, in first-seen order.
fn dedupe_defs(src: &[u8], raw_defs: &[RawDefEntry]) -> Vec<RawFunctionDef> {
    let mut order: Vec<(String, usize)> = Vec::new();
    let mut by_key: std::collections::HashMap<(String, usize), RawFunctionDef> =
        std::collections::HashMap::new();

    for entry in raw_defs {
        let Some((name, arity)) = parse_def_head(src, entry.def_node) else {
            continue;
        };
        let public = matches!(entry.keyword, "def" | "defmacro");
        let start_row = entry.def_node.start_position().row;
        let end_row = entry.def_node.end_position().row;
        let key = (name.clone(), arity);
        match by_key.get_mut(&key) {
            Some(existing) => {
                existing.start_row = existing.start_row.min(start_row);
                existing.end_row = existing.end_row.max(end_row);
            }
            None => {
                order.push(key.clone());
                by_key.insert(
                    key,
                    RawFunctionDef {
                        name,
                        arity,
                        start_row,
                        end_row,
                        public,
                    },
                );
            }
        }
    }

    order
        .into_iter()
        .filter_map(|key| by_key.remove(&key))
        .collect()
}

/// Parse a `def`/`defp`/`defmacro`/`defmacrop` call node's head (its first
/// argument) into `(name, arity)`. Handles the three head shapes the
/// grammar produces:
/// - `call` (`foo(a, b)`) -- name + named-child count of its `arguments`.
/// - `binary_operator` with a `when` guard (`foo(a, b) when a > 0`) --
///   recurse into `left`, ignoring the guard on `right`.
/// - bare `identifier` (`foo`, zero-arity without parens) -- arity 0.
fn parse_def_head(src: &[u8], def_node: Node) -> Option<(String, usize)> {
    let head = first_call_argument(def_node)?;
    parse_head(src, head)
}

fn parse_head(src: &[u8], head: Node) -> Option<(String, usize)> {
    match head.kind() {
        "call" => {
            let target = head.child_by_field_name("target")?;
            if target.kind() != "identifier" {
                return None;
            }
            let name = node_text(src, target);
            let arity = child_of_kind(head, "arguments")
                .map(|a| a.named_child_count())
                .unwrap_or(0);
            Some((name, arity))
        }
        "binary_operator" => {
            let operator = head.child_by_field_name("operator")?;
            if node_text(src, operator) != "when" {
                return None;
            }
            let left = head.child_by_field_name("left")?;
            parse_head(src, left)
        }
        "identifier" => Some((node_text(src, head), 0)),
        _ => None,
    }
}

/// Resolve a remote call's dotted target (`Bar.Sub` in `Bar.Sub.fun(...)`)
/// through `aliases`: replace the first segment if it's a known alias,
/// otherwise assume the text is already fully qualified.
fn resolve_alias(text: &str, aliases: &[(String, String)]) -> String {
    let (first, rest) = match text.split_once('.') {
        Some((first, rest)) => (first, Some(rest)),
        None => (text, None),
    };
    let Some((_, target)) = aliases.iter().find(|(name, _)| name == first) else {
        return text.to_string();
    };
    match rest {
        Some(rest) => format!("{target}.{rest}"),
        None => target.clone(),
    }
}

/// Named-argument count of a `call` node's `arguments` child, 0 if absent.
fn call_arity(node: Node) -> usize {
    child_of_kind(node, "arguments")
        .map(|a| a.named_child_count())
        .unwrap_or(0)
}

/// Walk a function body for call sites, attributing each to `caller`.
/// `x |> f(y)` is special-cased: `f`'s effective arity gets `+1` for the
/// piped-in value, at every stage of a chain (each `|>`'s `right` side gets
/// the bump; its `left` side is walked normally, which recurses into any
/// further `|>` stage the same way).
fn walk_calls(
    src: &[u8],
    node: Node,
    caller: &(String, usize),
    aliases: &[(String, String)],
    known_names: &std::collections::HashSet<&str>,
    out: &mut Vec<RawCallSite>,
) {
    if node.kind() == "binary_operator" {
        if let Some(operator) = node.child_by_field_name("operator") {
            if node_text(src, operator) == "|>" {
                if let (Some(left), Some(right)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) {
                    walk_calls(src, left, caller, aliases, known_names, out);
                    process_call_node(src, right, caller, aliases, known_names, 1, out);
                }
                return;
            }
        }
    }
    if node.kind() == "call" {
        process_call_node(src, node, caller, aliases, known_names, 0, out);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_calls(src, child, caller, aliases, known_names, out);
    }
}

/// Record `node` (a `call`) as a [`RawCallSite`] if it's a local call to a
/// known def or a remote (dotted) call, then recurse into its arguments to
/// find further nested calls -- regardless of whether this call itself was
/// recorded.
fn process_call_node(
    src: &[u8],
    node: Node,
    caller: &(String, usize),
    aliases: &[(String, String)],
    known_names: &std::collections::HashSet<&str>,
    extra_arity: usize,
    out: &mut Vec<RawCallSite>,
) {
    if let Some(target) = node.child_by_field_name("target") {
        match target.kind() {
            "identifier" => {
                let name = node_text(src, target);
                if known_names.contains(name.as_str()) {
                    let arity = call_arity(node) + extra_arity;
                    out.push(RawCallSite {
                        caller: caller.clone(),
                        target: RawCallTarget::Local { name, arity },
                        row: node.start_position().row,
                    });
                }
            }
            "dot" => {
                if let Some(left) = target.child_by_field_name("left") {
                    if left.kind() == "alias" {
                        let module_text = node_text(src, left);
                        let module = resolve_alias(&module_text, aliases);
                        if let Some(right) = target.child_by_field_name("right") {
                            let name = node_text(src, right);
                            let arity = call_arity(node) + extra_arity;
                            out.push(RawCallSite {
                                caller: caller.clone(),
                                target: RawCallTarget::Remote {
                                    module,
                                    name,
                                    arity,
                                },
                                row: node.start_position().row,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(args) = child_of_kind(node, "arguments") {
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            walk_calls(src, child, caller, aliases, known_names, out);
        }
    }
    // Special forms (`if`/`case`/`unless`/`with`/`for`/...) are `call`
    // nodes with a `do_block` sibling of `arguments`, not inside it --
    // still part of the current function's body, so keep walking.
    if let Some(body) = child_of_kind(node, "do_block") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            walk_calls(src, child, caller, aliases, known_names, out);
        }
    }
}

#[cfg(test)]
mod function_tests {
    use super::*;

    #[test]
    fn def_and_defp_public_flag_and_spans() {
        let modules = extract_functions(
            r#"
            defmodule MyApp.Accounts do
                def create(attrs) do
                    attrs
                end

                defp helper(x) do
                    x
                end
            end
            "#,
        );
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.module, "MyApp.Accounts");
        let create = m.functions.iter().find(|f| f.name == "create").unwrap();
        assert_eq!(create.arity, 1);
        assert!(create.public);
        let helper = m.functions.iter().find(|f| f.name == "helper").unwrap();
        assert_eq!(helper.arity, 1);
        assert!(!helper.public);
    }

    #[test]
    fn multi_clause_functions_dedupe_to_one_entry_spanning_all_clauses() {
        let modules = extract_functions(
            r#"
            defmodule MyApp.Accounts do
                def kind(0) do
                    :zero
                end

                def kind(n) do
                    :nonzero
                end
            end
            "#,
        );
        let m = &modules[0];
        let kind_defs: Vec<_> = m.functions.iter().filter(|f| f.name == "kind").collect();
        assert_eq!(kind_defs.len(), 1);
        assert_eq!(kind_defs[0].arity, 1);
        assert!(kind_defs[0].start_row < kind_defs[0].end_row);
    }

    #[test]
    fn when_guard_head_extracts_name_and_arity() {
        let modules = extract_functions(
            r#"
            defmodule MyApp.Accounts do
                def classify(a, b) when a > 0 do
                    b
                end
            end
            "#,
        );
        let m = &modules[0];
        let f = m.functions.iter().find(|f| f.name == "classify").unwrap();
        assert_eq!(f.arity, 2);
    }

    #[test]
    fn alias_as_rename_resolves_in_remote_calls() {
        let modules = extract_functions(
            r#"
            defmodule MyApp.Accounts do
                alias MyApp.Repo, as: R

                def create(attrs) do
                    R.insert(attrs)
                end
            end
            "#,
        );
        let m = &modules[0];
        assert_eq!(
            m.calls,
            vec![RawCallSite {
                caller: ("create".to_string(), 1),
                target: RawCallTarget::Remote {
                    module: "MyApp.Repo".to_string(),
                    name: "insert".to_string(),
                    arity: 1,
                },
                row: m.calls[0].row,
            }]
        );
    }

    #[test]
    fn multi_alias_group_resolves_each_alias_in_remote_calls() {
        let modules = extract_functions(
            r#"
            defmodule MyApp.Accounts do
                alias MyApp.Accounts.{User, Profile}

                def build(attrs) do
                    User.new(attrs)
                    Profile.new(attrs)
                end
            end
            "#,
        );
        let m = &modules[0];
        let modules_called: Vec<&str> = m
            .calls
            .iter()
            .map(|c| match &c.target {
                RawCallTarget::Remote { module, .. } => module.as_str(),
                RawCallTarget::Local { .. } => panic!("expected remote calls"),
            })
            .collect();
        assert_eq!(
            modules_called,
            vec!["MyApp.Accounts.User", "MyApp.Accounts.Profile"]
        );
    }

    #[test]
    fn local_call_only_counted_when_def_exists_in_module() {
        let modules = extract_functions(
            r#"
            defmodule MyApp.Accounts do
                def create(attrs) do
                    if valid?(attrs) do
                        helper(attrs)
                    end
                end

                defp helper(x) do
                    x
                end
            end
            "#,
        );
        let m = &modules[0];
        let local_names: Vec<&str> = m
            .calls
            .iter()
            .filter_map(|c| match &c.target {
                RawCallTarget::Local { name, .. } => Some(name.as_str()),
                RawCallTarget::Remote { .. } => None,
            })
            .collect();
        // `if` and `valid?` (no matching def) are not counted as local
        // calls; `helper` is, since a matching `defp helper/1` exists.
        assert_eq!(local_names, vec!["helper"]);
    }

    #[test]
    fn pipe_bumps_effective_arity_at_every_chain_stage() {
        let modules = extract_functions(
            r#"
            defmodule MyApp.Accounts do
                alias MyApp.Repo

                def create(attrs) do
                    attrs |> Repo.insert() |> finalize()
                end

                defp finalize(x) do
                    x
                end
            end
            "#,
        );
        let m = &modules[0];
        assert_eq!(m.calls.len(), 2);
        let remote = m
            .calls
            .iter()
            .find_map(|c| match &c.target {
                RawCallTarget::Remote {
                    module,
                    name,
                    arity,
                } => Some((module, name, *arity)),
                RawCallTarget::Local { .. } => None,
            })
            .unwrap();
        assert_eq!(
            remote,
            (&"MyApp.Repo".to_string(), &"insert".to_string(), 1)
        );
        let local = m
            .calls
            .iter()
            .find_map(|c| match &c.target {
                RawCallTarget::Local { name, arity } => Some((name, *arity)),
                RawCallTarget::Remote { .. } => None,
            })
            .unwrap();
        assert_eq!(local, (&"finalize".to_string(), 1));
    }

    #[test]
    fn nested_module_qualification_matches_defmodule_qualify() {
        let modules = extract_functions(
            r#"
            defmodule Foo do
                defmodule Bar do
                    def run(x) do
                        x
                    end
                end
            end
            "#,
        );
        let names: Vec<&str> = modules.iter().map(|m| m.module.as_str()).collect();
        assert_eq!(names, vec!["Foo", "Foo.Bar"]);
        let bar = modules.iter().find(|m| m.module == "Foo.Bar").unwrap();
        assert_eq!(bar.functions[0].name, "run");
    }

    #[test]
    fn call_outside_any_def_is_skipped() {
        let modules = extract_functions(
            r#"
            defmodule MyApp.Accounts do
                alias MyApp.Repo

                Repo.insert(%{})

                def create(attrs) do
                    attrs
                end
            end
            "#,
        );
        let m = &modules[0];
        assert!(m.calls.is_empty());
    }
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
