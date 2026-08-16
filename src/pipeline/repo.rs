//! [`GitRepo`]: the trait abstracting everything the pipeline needs from
//! git, plus [`FakeRepo`], an in-memory implementation for pipeline tests.
//! The real implementation ([`crate::pipeline::git2_repo::Git2Repo`]) lands
//! in a later milestone; keeping this trait string-based (no `git2::Oid`)
//! keeps `FakeRepo` trivial and git2 types out of the pure pipeline layers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::pipeline::error::Result;

/// How a file changed between the diff base and head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Present at head, absent at base.
    Added,
    /// Present at both; content differs.
    Modified,
    /// Present at base, absent at head.
    Deleted,
    /// Present at both under different paths; `from` is the base-side path.
    Renamed { from: PathBuf },
}

/// One changed file between the diff base and head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDelta {
    /// The file's path at head (or, for a pure deletion, at base).
    pub path: PathBuf,
    pub change: Change,
}

/// Everything the pipeline needs from a git repository. Implemented for real
/// by `Git2Repo` (milestone 12) and here by [`FakeRepo`] for tests.
pub trait GitRepo {
    /// Resolve the diff base to a merge-base oid (hex string) with HEAD:
    /// `base_override` if given, else `origin/HEAD`'s symbolic target, else
    /// `main`, else `master`.
    fn default_base_oid(&self, base_override: Option<&str>) -> Result<String>;

    /// Every file that differs between `base_oid` and HEAD, with rename
    /// detection on.
    fn changed_files(&self, base_oid: &str) -> Result<Vec<FileDelta>>;

    /// `path`'s content at `base_oid`, or `None` if it didn't exist there.
    fn base_blob(&self, base_oid: &str, path: &Path) -> Result<Option<String>>;

    /// `path`'s content in the checked-out worktree, or `None` if absent.
    fn head_content(&self, path: &Path) -> Result<Option<String>>;

    /// Every file tracked at HEAD (needed to parse unchanged files for the
    /// module-name table).
    fn list_tracked_files(&self) -> Result<Vec<PathBuf>>;
}

/// In-memory [`GitRepo`] for pipeline tests: scripted deltas plus base/head
/// content maps, no actual git access.
#[derive(Debug, Clone, Default)]
pub struct FakeRepo {
    /// Returned by `default_base_oid` when no override is given.
    pub default_base_oid: String,
    /// Returned verbatim by `changed_files`, regardless of `base_oid`.
    pub deltas: Vec<FileDelta>,
    /// Content keyed by path, as it existed at the diff base.
    pub base_files: HashMap<PathBuf, String>,
    /// Content keyed by path, as it exists in the worktree at head.
    pub head_files: HashMap<PathBuf, String>,
    /// Returned verbatim by `list_tracked_files`.
    pub tracked_files: Vec<PathBuf>,
}

impl GitRepo for FakeRepo {
    fn default_base_oid(&self, base_override: Option<&str>) -> Result<String> {
        Ok(base_override
            .map(str::to_string)
            .unwrap_or_else(|| self.default_base_oid.clone()))
    }

    fn changed_files(&self, _base_oid: &str) -> Result<Vec<FileDelta>> {
        Ok(self.deltas.clone())
    }

    fn base_blob(&self, _base_oid: &str, path: &Path) -> Result<Option<String>> {
        Ok(self.base_files.get(path).cloned())
    }

    fn head_content(&self, path: &Path) -> Result<Option<String>> {
        Ok(self.head_files.get(path).cloned())
    }

    fn list_tracked_files(&self) -> Result<Vec<PathBuf>> {
        Ok(self.tracked_files.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted scenario: base resolves to a fixed oid unless overridden,
    /// one added/one modified/one deleted/one renamed file, content maps for
    /// each side, and a tracked-files list including an untouched file.
    fn scenario() -> FakeRepo {
        let mut base_files = HashMap::new();
        base_files.insert(PathBuf::from("src/old_name.rs"), "old content".to_string());
        base_files.insert(PathBuf::from("src/modified.rs"), "before".to_string());
        base_files.insert(PathBuf::from("src/deleted.rs"), "gone".to_string());

        let mut head_files = HashMap::new();
        head_files.insert(PathBuf::from("src/new_name.rs"), "old content".to_string());
        head_files.insert(PathBuf::from("src/modified.rs"), "after".to_string());
        head_files.insert(PathBuf::from("src/added.rs"), "brand new".to_string());
        head_files.insert(PathBuf::from("src/unchanged.rs"), "steady".to_string());

        FakeRepo {
            default_base_oid: "deadbeef".to_string(),
            deltas: vec![
                FileDelta {
                    path: PathBuf::from("src/added.rs"),
                    change: Change::Added,
                },
                FileDelta {
                    path: PathBuf::from("src/modified.rs"),
                    change: Change::Modified,
                },
                FileDelta {
                    path: PathBuf::from("src/deleted.rs"),
                    change: Change::Deleted,
                },
                FileDelta {
                    path: PathBuf::from("src/new_name.rs"),
                    change: Change::Renamed {
                        from: PathBuf::from("src/old_name.rs"),
                    },
                },
            ],
            base_files,
            head_files,
            tracked_files: vec![
                PathBuf::from("src/new_name.rs"),
                PathBuf::from("src/modified.rs"),
                PathBuf::from("src/added.rs"),
                PathBuf::from("src/unchanged.rs"),
            ],
        }
    }

    #[test]
    fn default_base_oid_falls_back_to_configured_default() {
        let repo = scenario();
        assert_eq!(repo.default_base_oid(None).unwrap(), "deadbeef");
    }

    #[test]
    fn default_base_oid_prefers_override() {
        let repo = scenario();
        assert_eq!(
            repo.default_base_oid(Some("custom-ref")).unwrap(),
            "custom-ref"
        );
    }

    #[test]
    fn changed_files_returns_scripted_deltas() {
        let repo = scenario();
        let deltas = repo.changed_files("deadbeef").unwrap();
        assert_eq!(deltas.len(), 4);
        assert!(deltas.contains(&FileDelta {
            path: PathBuf::from("src/new_name.rs"),
            change: Change::Renamed {
                from: PathBuf::from("src/old_name.rs")
            },
        }));
    }

    #[test]
    fn base_blob_returns_content_or_none() {
        let repo = scenario();
        assert_eq!(
            repo.base_blob("deadbeef", Path::new("src/deleted.rs"))
                .unwrap(),
            Some("gone".to_string())
        );
        assert_eq!(
            repo.base_blob("deadbeef", Path::new("src/added.rs"))
                .unwrap(),
            None
        );
    }

    #[test]
    fn head_content_returns_content_or_none() {
        let repo = scenario();
        assert_eq!(
            repo.head_content(Path::new("src/added.rs")).unwrap(),
            Some("brand new".to_string())
        );
        assert_eq!(
            repo.head_content(Path::new("src/deleted.rs")).unwrap(),
            None
        );
    }

    #[test]
    fn list_tracked_files_includes_unchanged_files() {
        let repo = scenario();
        let tracked = repo.list_tracked_files().unwrap();
        assert!(tracked.contains(&PathBuf::from("src/unchanged.rs")));
        assert_eq!(tracked.len(), 4);
    }
}
