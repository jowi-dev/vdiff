//! Resolve a Rust source file's crate name from the nearest ancestor
//! `Cargo.toml`'s `[package] name`, read through [`GitRepo::head_content`]
//! (the worktree) so [`crate::pipeline::repo::FakeRepo`] can script it in
//! tests without any real filesystem I/O.
//!
//! Walking starts at the source file's own directory and proceeds upward
//! one directory at a time. The first `Cargo.toml` found that has a
//! `[package]` table wins; a `Cargo.toml` with no `[package]` table (a
//! workspace root with only a `[workspace]` section) is skipped and the
//! walk continues further up -- an unusual layout (nested workspaces), but
//! keeps a member crate's own manifest from ever losing to an enclosing
//! workspace-only one. Returns `None` if no `[package] name` is found
//! anywhere above the file, letting the caller fall back to the directory
//! heuristic ([`crate::graph::builder::rust_crate_root_dir`]'s last
//! component).

use std::path::Path;

use crate::pipeline::repo::GitRepo;

/// Find `path`'s crate name by walking up from its directory looking for a
/// `Cargo.toml` with a `[package] name`. See the module docs for the exact
/// walk/skip rules.
pub fn crate_name_for(repo: &dyn GitRepo, path: &Path) -> Option<String> {
    let mut dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    loop {
        let manifest = dir.join("Cargo.toml");
        if let Ok(Some(content)) = repo.head_content(&manifest) {
            if let Some(name) = package_name(&content) {
                return Some(name);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The slice of `Cargo.toml` this module cares about: just `[package]
/// name`, ignoring everything else (dependencies, `[workspace]`, ...).
#[derive(serde::Deserialize)]
struct CargoManifest {
    package: Option<CargoPackage>,
}

#[derive(serde::Deserialize)]
struct CargoPackage {
    name: Option<String>,
}

/// Parse `content` as TOML and pull out `[package] name`, if present.
fn package_name(content: &str) -> Option<String> {
    let manifest: CargoManifest = toml::from_str(content).ok()?;
    manifest.package?.name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::repo::FakeRepo;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn repo_with(files: &[(&str, &str)]) -> FakeRepo {
        let mut head_files = HashMap::new();
        for (path, content) in files {
            head_files.insert(PathBuf::from(path), content.to_string());
        }
        FakeRepo {
            head_files,
            ..Default::default()
        }
    }

    #[test]
    fn finds_nearest_ancestor_cargo_toml_package_name() {
        let repo = repo_with(&[(
            "backend/Cargo.toml",
            "[package]\nname = \"myapi\"\nversion = \"0.1.0\"\n",
        )]);
        assert_eq!(
            crate_name_for(&repo, Path::new("backend/src/foo/bar.rs")),
            Some("myapi".to_string())
        );
    }

    #[test]
    fn skips_workspace_only_manifest_and_keeps_walking() {
        let repo = repo_with(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"backend\"]\n"),
            (
                "backend/Cargo.toml",
                "[package]\nname = \"myapi\"\nversion = \"0.1.0\"\n",
            ),
        ]);
        assert_eq!(
            crate_name_for(&repo, Path::new("backend/src/lib.rs")),
            Some("myapi".to_string())
        );
    }

    #[test]
    fn returns_none_when_no_cargo_toml_found() {
        let repo = repo_with(&[]);
        assert_eq!(crate_name_for(&repo, Path::new("backend/src/lib.rs")), None);
    }

    #[test]
    fn returns_none_when_only_workspace_manifest_exists() {
        let repo = repo_with(&[("Cargo.toml", "[workspace]\nmembers = [\"backend\"]\n")]);
        assert_eq!(crate_name_for(&repo, Path::new("backend/src/lib.rs")), None);
    }
}
