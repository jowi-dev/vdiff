//! [`RustExtract`]: a [`ModuleExtractor`] over `syn`'s AST. Good enough for
//! visualization, not a full name resolver: `use` paths are stored as
//! written (e.g. `"crate::pipeline::repo::GitRepo"`, `"serde::Serialize"`);
//! turning those into resolved edges is [`crate::pipeline::resolve`]'s job.

use std::path::Path;

use syn::{Item, UseTree};

use crate::graph::model::DepKind;
use crate::pipeline::extract::{DepRef, ModuleDef, ModuleExtractor};

/// Extracts Rust modules and `use` dependencies via `syn`.
pub struct RustExtract;

impl ModuleExtractor for RustExtract {
    fn extract(&self, _path: &Path, source: &str) -> Vec<ModuleDef> {
        let Ok(file) = syn::parse_file(source) else {
            return Vec::new();
        };
        let mut defs = vec![ModuleDef {
            name: String::new(),
            dep_refs: use_refs(&file.items),
        }];
        collect_inline_mods(&file.items, "", &mut defs);
        defs
    }
}

/// Recursively collect `ModuleDef`s for every inline `mod name { ... }` in
/// `items` (out-of-line `mod name;` declarations have no body to recurse
/// into, so they don't produce a `ModuleDef` here -- resolving those against
/// the filesystem is the builder's job in a later milestone).
fn collect_inline_mods(items: &[Item], prefix: &str, defs: &mut Vec<ModuleDef>) {
    for item in items {
        let Item::Mod(module) = item else { continue };
        let Some((_, inner_items)) = &module.content else {
            continue;
        };
        let name = qualify(prefix, &module.ident.to_string());
        defs.push(ModuleDef {
            name: name.clone(),
            dep_refs: use_refs(inner_items),
        });
        collect_inline_mods(inner_items, &name, defs);
    }
}

/// Join a module path prefix and a segment with `::`, omitting the
/// separator when `prefix` is empty.
fn qualify(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_string()
    } else {
        format!("{prefix}::{segment}")
    }
}

/// Every `use` path directly inside `items` (not inside nested `mod`
/// blocks, which collect their own via [`collect_inline_mods`]).
fn use_refs(items: &[Item]) -> Vec<DepRef> {
    let mut refs = Vec::new();
    for item in items {
        if let Item::Use(use_item) = item {
            flatten_use_tree(&use_item.tree, String::new(), &mut refs);
        }
    }
    refs
}

/// Flatten a `use` tree into one [`DepRef`] per leaf, handling groups
/// (`use a::{b, c}`), globs (`use a::*`), and renames (`use a::b as c`).
fn flatten_use_tree(tree: &UseTree, prefix: String, out: &mut Vec<DepRef>) {
    match tree {
        UseTree::Path(path) => {
            flatten_use_tree(&path.tree, qualify(&prefix, &path.ident.to_string()), out);
        }
        UseTree::Name(name) => {
            out.push(dep_ref(qualify(&prefix, &name.ident.to_string())));
        }
        UseTree::Rename(rename) => {
            // Store the original path, not the local alias -- resolution
            // cares about the real target module.
            out.push(dep_ref(qualify(&prefix, &rename.ident.to_string())));
        }
        UseTree::Glob(_) => {
            // `use a::b::*;` depends on the module `a::b` itself.
            out.push(dep_ref(prefix));
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                flatten_use_tree(tree, prefix.clone(), out);
            }
        }
    }
}

fn dep_ref(name: String) -> DepRef {
    DepRef {
        name,
        kind: DepKind::Use,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn extract(source: &str) -> Vec<ModuleDef> {
        RustExtract.extract(Path::new("src/lib.rs"), source)
    }

    #[test]
    fn root_module_collects_top_level_use() {
        let defs = extract("use crate::pipeline::repo::GitRepo;\nfn f() {}");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "");
        assert_eq!(
            defs[0].dep_refs,
            vec![dep_ref("crate::pipeline::repo::GitRepo".to_string())]
        );
    }

    #[test]
    fn nested_inline_mods_get_qualified_names_and_own_uses() {
        let source = r#"
            mod foo {
                use outer::Thing;
                mod bar {
                    use baz::Qux;
                }
            }
        "#;
        let defs = extract(source);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["", "foo", "foo::bar"]);

        let foo = defs.iter().find(|d| d.name == "foo").unwrap();
        assert_eq!(foo.dep_refs, vec![dep_ref("outer::Thing".to_string())]);

        let bar = defs.iter().find(|d| d.name == "foo::bar").unwrap();
        assert_eq!(bar.dep_refs, vec![dep_ref("baz::Qux".to_string())]);
    }

    #[test]
    fn out_of_line_mod_declaration_produces_no_def() {
        let defs = extract("mod other_file;");
        assert_eq!(defs.len(), 1, "only the root ModuleDef");
        assert_eq!(defs[0].name, "");
    }

    #[test]
    fn grouped_use_flattens_to_one_ref_per_leaf() {
        let defs = extract("use std::{fmt, collections::HashMap};");
        assert_eq!(
            defs[0].dep_refs,
            vec![
                dep_ref("std::fmt".to_string()),
                dep_ref("std::collections::HashMap".to_string()),
            ]
        );
    }

    #[test]
    fn glob_use_refers_to_the_globbed_module() {
        let defs = extract("use foo::bar::*;");
        assert_eq!(defs[0].dep_refs, vec![dep_ref("foo::bar".to_string())]);
    }

    #[test]
    fn renamed_use_stores_the_original_path() {
        let defs = extract("use foo::Bar as Baz;");
        assert_eq!(defs[0].dep_refs, vec![dep_ref("foo::Bar".to_string())]);
    }

    #[test]
    fn unparseable_source_yields_no_defs() {
        let defs = extract("this is not { rust");
        assert!(defs.is_empty());
    }
}
