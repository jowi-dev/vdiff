//! [`Git2Repo`]: the real [`GitRepo`] implementation, backed by `git2`.
//!
//! Default-branch detection: `origin/HEAD`'s symbolic target if present,
//! else `refs/remotes/origin/main`, `refs/heads/main`,
//! `refs/remotes/origin/master`, `refs/heads/master` in that order --
//! whichever exists first. `base_override`, when given, is tried directly
//! via `revparse_single` instead.
//!
//! `head_content`/`list_tracked_files` read the checked-out worktree and
//! the HEAD commit's tree respectively; `base_blob`/`base_blob_oid` read
//! `base_oid`'s tree. Known limitation: `head_blob_oid` is sourced from the
//! HEAD commit's tree, not a live hash of the worktree file -- if the
//! worktree has uncommitted changes beyond HEAD, `head_content` still
//! returns the accurate live content for parsing, but the recorded blob id
//! won't match it exactly. Fine for the reviewed-branch workflow this tool
//! targets (committed changes), a known gap otherwise.

use std::path::{Path, PathBuf};

use git2::{Delta, DiffFindOptions, ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult};

use crate::pipeline::error::{PipelineError, Result};
use crate::pipeline::repo::{Change, FileDelta, GitRepo};

/// A real git repository, opened via `git2`.
pub struct Git2Repo {
    repo: Repository,
}

impl Git2Repo {
    /// Discover and open the repository containing `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let repo = Repository::discover(path)?;
        Ok(Self { repo })
    }

    /// Resolve `base_override` (if given) or the default branch to a ref
    /// name/revspec `revparse_single` can look up.
    fn base_revspec(&self, base_override: Option<&str>) -> Result<String> {
        if let Some(base) = base_override {
            return Ok(base.to_string());
        }
        if let Ok(origin_head) = self.repo.find_reference("refs/remotes/origin/HEAD") {
            if let Ok(Some(target)) = origin_head.symbolic_target() {
                return Ok(target.to_string());
            }
        }
        for candidate in [
            "refs/remotes/origin/main",
            "refs/heads/main",
            "refs/remotes/origin/master",
            "refs/heads/master",
        ] {
            if self.repo.find_reference(candidate).is_ok() {
                return Ok(candidate.to_string());
            }
        }
        Err(PipelineError::NoBaseRef)
    }

    fn tree_at(&self, oid_hex: &str) -> Result<git2::Tree<'_>> {
        let oid = Oid::from_str(oid_hex)?;
        let commit = self.repo.find_commit(oid)?;
        Ok(commit.tree()?)
    }

    fn read_io(path: &Path) -> Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(Some(content)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(PipelineError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

impl GitRepo for Git2Repo {
    fn default_base_oid(&self, base_override: Option<&str>) -> Result<String> {
        let head = self.repo.head()?.peel_to_commit()?;
        let base_revspec = self.base_revspec(base_override)?;
        let base_commit = self.repo.revparse_single(&base_revspec)?.peel_to_commit()?;
        let merge_base = self.repo.merge_base(head.id(), base_commit.id())?;
        Ok(merge_base.to_string())
    }

    fn changed_files(&self, base_oid: &str) -> Result<Vec<FileDelta>> {
        let base_tree = self.tree_at(base_oid)?;
        let head_tree = self.repo.head()?.peel_to_tree()?;
        let mut diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)?;
        let mut find_opts = DiffFindOptions::new();
        find_opts.renames(true);
        diff.find_similar(Some(&mut find_opts))?;

        let mut deltas = Vec::new();
        for delta in diff.deltas() {
            let new_path = delta.new_file().path().map(Path::to_path_buf);
            let old_path = delta.old_file().path().map(Path::to_path_buf);
            match delta.status() {
                Delta::Added => {
                    if let Some(path) = new_path {
                        deltas.push(FileDelta {
                            path,
                            change: Change::Added,
                        });
                    }
                }
                Delta::Deleted => {
                    if let Some(path) = old_path {
                        deltas.push(FileDelta {
                            path,
                            change: Change::Deleted,
                        });
                    }
                }
                Delta::Renamed => {
                    if let (Some(path), Some(from)) = (new_path, old_path) {
                        deltas.push(FileDelta {
                            path,
                            change: Change::Renamed { from },
                        });
                    }
                }
                // Modified, Copied, Typechange, and anything else observed
                // between two trees: treat as a content modification at
                // the new path.
                _ => {
                    if let Some(path) = new_path {
                        deltas.push(FileDelta {
                            path,
                            change: Change::Modified,
                        });
                    }
                }
            }
        }
        Ok(deltas)
    }

    fn base_blob(&self, base_oid: &str, path: &Path) -> Result<Option<String>> {
        let tree = self.tree_at(base_oid)?;
        let Ok(entry) = tree.get_path(path) else {
            return Ok(None);
        };
        let object = entry.to_object(&self.repo)?;
        let Some(blob) = object.as_blob() else {
            return Ok(None);
        };
        Ok(Some(String::from_utf8_lossy(blob.content()).into_owned()))
    }

    fn head_content(&self, path: &Path) -> Result<Option<String>> {
        let workdir = self.repo.workdir().ok_or(PipelineError::NoBaseRef)?;
        Self::read_io(&workdir.join(path))
    }

    fn list_tracked_files(&self) -> Result<Vec<PathBuf>> {
        let head_tree = self.repo.head()?.peel_to_tree()?;
        let mut files = Vec::new();
        head_tree.walk(TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(ObjectType::Blob) {
                if let Ok(name) = entry.name() {
                    files.push(PathBuf::from(format!("{root}{name}")));
                }
            }
            TreeWalkResult::Ok
        })?;
        Ok(files)
    }

    fn base_blob_oid(&self, base_oid: &str, path: &Path) -> Result<Option<String>> {
        let tree = self.tree_at(base_oid)?;
        Ok(tree.get_path(path).ok().map(|entry| entry.id().to_string()))
    }

    fn head_blob_oid(&self, path: &Path) -> Result<Option<String>> {
        let head_tree = self.repo.head()?.peel_to_tree()?;
        Ok(head_tree
            .get_path(path)
            .ok()
            .map(|entry| entry.id().to_string()))
    }
}
