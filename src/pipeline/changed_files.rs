//! [`ChangeSet`]: a pure lookup from repo-relative path to [`GitStatus`],
//! built from the [`FileDelta`]s a [`crate::pipeline::repo::GitRepo`]
//! reports.
//!
//! Rename handling: a [`Change::Renamed`] only registers a status for its
//! new path (as [`GitStatus::Modified`]) -- the old path is not surfaced as
//! a separate entry, so it reports [`GitStatus::Unchanged`] via
//! [`ChangeSet::status_for`] unless some other delta touches it. This keeps
//! a rename from producing two nodes (a phantom deleted one at the old path
//! and a modified one at the new path); the builder (milestone 10) only
//! ever sees the new path in the tracked-files list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::graph::model::{FileStats, GitStatus};
use crate::pipeline::repo::{Change, FileDelta};

/// A path's [`GitStatus`] plus the [`FileStats`] its delta carried, if any.
#[derive(Debug, Clone, Copy)]
struct ChangeEntry {
    status: GitStatus,
    stats: FileStats,
}

/// A path -> status/stats lookup built from a repo's changed files.
#[derive(Debug, Clone, Default)]
pub struct ChangeSet {
    entries: HashMap<PathBuf, ChangeEntry>,
}

impl ChangeSet {
    /// Build a `ChangeSet` from a [`GitRepo`](crate::pipeline::repo::GitRepo)'s
    /// reported deltas.
    pub fn from_deltas(deltas: Vec<FileDelta>) -> Self {
        let entries = deltas
            .into_iter()
            .map(|delta| {
                let entry = ChangeEntry {
                    status: status_of(&delta.change),
                    stats: delta.stats,
                };
                (delta.path, entry)
            })
            .collect();
        ChangeSet { entries }
    }

    /// `path`'s status relative to the diff base. [`GitStatus::Unchanged`]
    /// if `path` wasn't touched (or is only known as the old side of a
    /// rename -- see the module docs).
    pub fn status_for(&self, path: &Path) -> GitStatus {
        self.entries
            .get(path)
            .map(|entry| entry.status)
            .unwrap_or(GitStatus::Unchanged)
    }

    /// `path`'s line-count stats, or `None` if `path` wasn't touched (or is
    /// only known as the old side of a rename -- see the module docs) --
    /// feeds [`crate::graph::model::FileRef::stats`], where `None` means
    /// the same thing: not part of the change set.
    pub fn stats_for(&self, path: &Path) -> Option<FileStats> {
        self.entries.get(path).map(|entry| entry.stats)
    }
}

/// Map a [`Change`] to the [`GitStatus`] its new-side path should report.
fn status_of(change: &Change) -> GitStatus {
    match change {
        Change::Added => GitStatus::Added,
        Change::Modified => GitStatus::Modified,
        Change::Deleted => GitStatus::Deleted,
        Change::Renamed { .. } => GitStatus::Modified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::FileStats;

    fn delta(path: &str, change: Change) -> FileDelta {
        FileDelta {
            path: PathBuf::from(path),
            change,
            stats: FileStats::default(),
        }
    }

    #[test]
    fn maps_each_change_kind_to_its_status() {
        let set = ChangeSet::from_deltas(vec![
            delta("added.rs", Change::Added),
            delta("modified.rs", Change::Modified),
            delta("deleted.rs", Change::Deleted),
        ]);
        assert_eq!(set.status_for(Path::new("added.rs")), GitStatus::Added);
        assert_eq!(
            set.status_for(Path::new("modified.rs")),
            GitStatus::Modified
        );
        assert_eq!(set.status_for(Path::new("deleted.rs")), GitStatus::Deleted);
    }

    #[test]
    fn unknown_path_is_unchanged() {
        let set = ChangeSet::from_deltas(vec![delta("added.rs", Change::Added)]);
        assert_eq!(
            set.status_for(Path::new("never_touched.rs")),
            GitStatus::Unchanged
        );
    }

    #[test]
    fn stats_for_returns_the_deltas_stats() {
        let mut delta = delta("modified.rs", Change::Modified);
        delta.stats = FileStats {
            added: 4,
            deleted: 2,
            binary: false,
        };
        let set = ChangeSet::from_deltas(vec![delta]);
        assert_eq!(
            set.stats_for(Path::new("modified.rs")),
            Some(FileStats {
                added: 4,
                deleted: 2,
                binary: false,
            })
        );
    }

    #[test]
    fn stats_for_unknown_path_is_none() {
        let set = ChangeSet::from_deltas(vec![delta("added.rs", Change::Added)]);
        assert_eq!(set.stats_for(Path::new("never_touched.rs")), None);
    }

    #[test]
    fn rename_reports_modified_at_new_path_and_unchanged_at_old_path() {
        let set = ChangeSet::from_deltas(vec![delta(
            "new_name.rs",
            Change::Renamed {
                from: PathBuf::from("old_name.rs"),
            },
        )]);
        assert_eq!(
            set.status_for(Path::new("new_name.rs")),
            GitStatus::Modified
        );
        assert_eq!(
            set.status_for(Path::new("old_name.rs")),
            GitStatus::Unchanged,
            "old path is not surfaced as its own node"
        );
    }
}
