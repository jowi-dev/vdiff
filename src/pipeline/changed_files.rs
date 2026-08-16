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

use crate::graph::model::GitStatus;
use crate::pipeline::repo::{Change, FileDelta};

/// A path -> [`GitStatus`] lookup built from a repo's changed files.
#[derive(Debug, Clone, Default)]
pub struct ChangeSet {
    statuses: HashMap<PathBuf, GitStatus>,
}

impl ChangeSet {
    /// Build a `ChangeSet` from a [`GitRepo`](crate::pipeline::repo::GitRepo)'s
    /// reported deltas.
    pub fn from_deltas(deltas: Vec<FileDelta>) -> Self {
        let statuses = deltas
            .into_iter()
            .map(|delta| (delta.path, status_of(&delta.change)))
            .collect();
        ChangeSet { statuses }
    }

    /// `path`'s status relative to the diff base. [`GitStatus::Unchanged`]
    /// if `path` wasn't touched (or is only known as the old side of a
    /// rename -- see the module docs).
    pub fn status_for(&self, path: &Path) -> GitStatus {
        self.statuses
            .get(path)
            .copied()
            .unwrap_or(GitStatus::Unchanged)
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

    fn delta(path: &str, change: Change) -> FileDelta {
        FileDelta {
            path: PathBuf::from(path),
            change,
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
