//! [`Git2Repo`]: the real [`GitRepo`] implementation, backed by `git2`.
//!
//! Default-branch detection: `origin/HEAD`'s symbolic target if present,
//! else `refs/remotes/origin/main`, `refs/heads/main`,
//! `refs/remotes/origin/master`, `refs/heads/master` in that order --
//! whichever exists first. `base_override`, when given, is tried directly
//! via `revparse_single` instead.
//!
//! vdiff's change set is `git diff <base>` -- `base_oid` vs. the working
//! directory (including staged changes and untracked files), not vs. the
//! HEAD commit -- everywhere, on purpose: the embedded nvim pane makes
//! editing-during-review a first-class flow, so a dirty worktree has to
//! show up as changes rather than being invisible in the graph while
//! still visible as diff marks in an opened file. `changed_files` uses
//! `diff_tree_to_workdir_with_index` accordingly; `head_content` already
//! read the worktree (that part was always right); `head_blob_oid` now
//! hashes the worktree file's live bytes (`Oid::hash_object`, no object-db
//! write) rather than reading the HEAD tree, so it agrees with
//! `head_content`/`changed_files` on what "exists at head" means -- a
//! worktree-only new file gets `head_blob_oid = Some(..)` and is never
//! mistaken for deleted. `list_tracked_files`/`base_blob`/`base_blob_oid`
//! still read the HEAD tree/`base_oid`'s tree respectively -- both
//! unambiguous, unaffected by this distinction. `head_content` and
//! `base_blob` both read raw bytes and lossy-decode as UTF-8
//! (`String::from_utf8_lossy`), so a non-UTF8 file (a binary asset)
//! degrades to garbage text for extraction rather than erroring
//! `build_graph` out entirely.
//!
//! `changed_files` never reports `.git` or `.claude` (vdiff's own
//! worktree/config directory) -- see `is_ignored_path`. This matters beyond
//! tidiness: a nested `git worktree add` checkout under, say,
//! `.claude/worktrees/<name>` has its own gitlink `.git` *file*, which makes
//! libgit2 treat that directory as a repo boundary it won't recurse into,
//! surfacing the directory itself as one opaque untracked entry rather than
//! the files inside it (issue #9). `changed_files` also drops any entry
//! that resolves to a directory on disk (belt-and-suspenders for the same
//! failure mode under a different name), and every worktree-byte read
//! (`read_io`, `head_blob_oid`) treats "not found", "is a directory", and
//! "permission denied" the same way: `None`, not an error -- so an
//! unreadable or directory-shaped entry that somehow slips past the
//! `changed_files` filter still can't abort `build_graph`.

use std::path::{Path, PathBuf};

use git2::{
    Delta, DiffFindOptions, DiffOptions, ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult,
};

use crate::pipeline::error::{PipelineError, Result};
use crate::pipeline::repo::{Change, FileDelta, GitRepo};

/// A real git repository, opened via `git2`.
pub struct Git2Repo {
    repo: Repository,
}

impl Git2Repo {
    /// Discover and open the repository containing `path`. Fails with
    /// [`PipelineError::NotAGitRepo`] (a friendly message, not a raw
    /// `git2::Error`) if `path` and none of its ancestors are a git
    /// repository.
    pub fn open(path: &Path) -> Result<Self> {
        let repo = Repository::discover(path).map_err(|_| PipelineError::NotAGitRepo {
            path: path.to_path_buf(),
        })?;
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

    /// Whether `path`'s first component is a directory the change-set walk
    /// must never surface: `.git` (already excluded by libgit2's own
    /// workdir scan, which hardcodes a skip for it) and `.claude` (vdiff's
    /// own worktree/config directory -- see the module doc for why this
    /// needs the same treatment: a nested `git worktree add` checkout
    /// living under `.claude/worktrees/...` has its own gitlink `.git`
    /// file, which makes libgit2 treat that directory as a repo boundary
    /// it won't recurse into, surfacing the directory itself as one opaque
    /// untracked entry instead of the files inside it).
    fn is_ignored_path(path: &Path) -> bool {
        matches!(
            path.components().next(),
            Some(std::path::Component::Normal(name))
                if name == ".git" || name == ".claude"
        )
    }

    fn tree_at(&self, oid_hex: &str) -> Result<git2::Tree<'_>> {
        let oid = Oid::from_str(oid_hex)?;
        let commit = self.repo.find_commit(oid)?;
        Ok(commit.tree()?)
    }

    /// Read `path`'s raw bytes and lossy-decode as UTF-8 -- mirrors
    /// `base_blob`'s handling of git blob content, so a non-UTF8 file (a
    /// binary asset, say) degrades to garbage text instead of erroring the
    /// whole pipeline out. The extractors for both languages already treat
    /// unparseable source as "no defs", so lossy-decoded binary content
    /// just becomes a childless leaf node.
    fn read_io(path: &Path) -> Result<Option<String>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            Err(err) if is_unreadable_entry(&err) => Ok(None),
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
        let base_commit = self
            .repo
            .revparse_single(&base_revspec)
            .and_then(|obj| obj.peel_to_commit())
            .map_err(|_| PipelineError::BaseRefNotFound {
                base: base_revspec.clone(),
            })?;
        let merge_base = self.repo.merge_base(head.id(), base_commit.id())?;
        Ok(merge_base.to_string())
    }

    fn changed_files(&self, base_oid: &str) -> Result<Vec<FileDelta>> {
        let base_tree = self.tree_at(base_oid)?;
        // `git diff <base>` semantics: base tree vs. working directory,
        // including the index (staged changes) and untracked files -- see
        // the module doc for why this, not base-vs-HEAD, is vdiff's change
        // set. `recurse_untracked_dirs` matters too: without it, a brand
        // new untracked *directory* shows up as one opaque delta for the
        // directory itself rather than one per file inside it.
        let mut opts = DiffOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let mut diff = self
            .repo
            .diff_tree_to_workdir_with_index(Some(&base_tree), Some(&mut opts))?;
        let mut find_opts = DiffFindOptions::new();
        find_opts.renames(true);
        diff.find_similar(Some(&mut find_opts))?;

        let workdir = self.repo.workdir();
        let mut deltas = Vec::new();
        for delta in diff.deltas() {
            let new_path = delta.new_file().path().map(Path::to_path_buf);
            let old_path = delta.old_file().path().map(Path::to_path_buf);

            // `.git`/`.claude` are never real change-set entries (see
            // `is_ignored_path`), and a directory -- or anything else that
            // can't be stat'd, e.g. a permission-denied entry -- can't be
            // read as a file's content downstream, so it must be skipped
            // here rather than surfacing as a `FileDelta` that later blows
            // up `load_content`/`head_blob_oid`.
            let is_unusable = |path: &Option<PathBuf>| {
                let Some(path) = path else { return false };
                if Self::is_ignored_path(path) {
                    return true;
                }
                match workdir {
                    Some(workdir) => {
                        matches!(std::fs::metadata(workdir.join(path)), Ok(meta) if meta.is_dir())
                    }
                    None => false,
                }
            };
            if is_unusable(&new_path) || is_unusable(&old_path) {
                continue;
            }

            match delta.status() {
                // `Untracked` -- a brand-new file with no index entry at
                // all -- is reported distinctly from `Added` (present in
                // the index/workdir but absent from the diff base's tree)
                // by `diff_tree_to_workdir_with_index`; both mean the same
                // thing for vdiff's purposes.
                Delta::Added | Delta::Untracked => {
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

    /// Hashes the worktree file's live bytes (`git2::Oid::hash_object` --
    /// computes the blob id a `git add` of this content would produce,
    /// without writing anything to the object database) rather than
    /// reading the HEAD tree, so this agrees with `head_content` and
    /// `changed_files`'s `git diff <base>` semantics on what "exists at
    /// head" means (see the module doc): a worktree-only new file gets
    /// `Some(oid)` here, not `None` as it would from the HEAD tree, and
    /// `None` means "absent from the worktree", not "absent from HEAD".
    fn head_blob_oid(&self, path: &Path) -> Result<Option<String>> {
        let Some(workdir) = self.repo.workdir() else {
            return Ok(None);
        };
        match std::fs::read(workdir.join(path)) {
            Ok(bytes) => Ok(Some(
                Oid::hash_object(ObjectType::Blob, &bytes)?.to_string(),
            )),
            Err(err) if is_unreadable_entry(&err) => Ok(None),
            Err(source) => Err(PipelineError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn git_dir(&self) -> PathBuf {
        self.repo.path().to_path_buf()
    }
}

/// Whether an I/O error reading a worktree path means "there's nothing
/// file-shaped here" rather than a real failure: absent (`NotFound`), a
/// directory (`IsADirectory` -- the `changed_files` filter should already
/// keep these out, but this is the last line of defense against `build_graph`
/// aborting over one), or permission-denied. Every [`GitRepo`] method that
/// reads worktree bytes treats these the same way `NotFound` always has:
/// `None`, not an error that kills the whole graph build.
fn is_unreadable_entry(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::IsADirectory
            | std::io::ErrorKind::PermissionDenied
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "vdiff tests")
            .env("GIT_AUTHOR_EMAIL", "vdiff-tests@example.com")
            .env("GIT_COMMITTER_NAME", "vdiff tests")
            .env("GIT_COMMITTER_EMAIL", "vdiff-tests@example.com")
            .status()
            .unwrap_or_else(|err| panic!("failed to run git {args:?}: {err}"));
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A tempdir with no `.git` anywhere above it must fail to open with a
    /// friendly [`PipelineError::NotAGitRepo`], not a raw `git2::Error`
    /// (whose `Display` impl tacks on an internal `class=...; code=...`
    /// suffix).
    #[test]
    fn opening_a_non_repo_directory_is_a_friendly_error() {
        let tmp = TempDir::new().expect("create tempdir");
        let err = match Git2Repo::open(tmp.path()) {
            Ok(_) => panic!("no .git anywhere above this tempdir"),
            Err(err) => err,
        };
        assert!(
            matches!(err, PipelineError::NotAGitRepo { .. }),
            "expected NotAGitRepo, got {err}"
        );
        assert!(
            !err.to_string().contains("class="),
            "message leaked git2 internals: {err}"
        );
    }

    /// `--base nonexistent-ref` must fail with a friendly
    /// [`PipelineError::BaseRefNotFound`] naming the ref, not a raw
    /// `git2::Error`.
    #[test]
    fn nonexistent_base_ref_is_a_friendly_error_naming_the_ref() {
        let tmp = TempDir::new().expect("create tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);

        let repo = Git2Repo::open(dir).expect("open fixture repo");
        let err = repo
            .default_base_oid(Some("nonexistent-ref"))
            .expect_err("no such ref");
        match err {
            PipelineError::BaseRefNotFound { base } => assert_eq!(base, "nonexistent-ref"),
            other => panic!("expected BaseRefNotFound, got {other:?}"),
        }
    }

    /// A detached-HEAD checkout must still resolve `default_base_oid` --
    /// `HEAD` points directly at a commit rather than a branch, but `git2`'s
    /// `repo.head()` peels to that commit exactly the same either way.
    #[test]
    fn detached_head_still_resolves_default_base_oid() {
        let tmp = TempDir::new().expect("create tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);
        std::fs::write(dir.join("a.txt"), "hi again\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "second"]);
        git(dir, &["checkout", "--detach", "HEAD"]);

        let repo = Git2Repo::open(dir).expect("open fixture repo");
        let result = repo.default_base_oid(Some("main"));
        assert!(
            result.is_ok(),
            "detached HEAD must still resolve a base oid: {result:?}"
        );
    }

    /// Reproduces issue #9: a nested git worktree (`.git` there is a gitlink
    /// *file*, not a directory) inside the repo makes libgit2's untracked
    /// scan surface the worktree's directory itself as one opaque entry
    /// (it won't recurse into a directory that looks like a repo boundary,
    /// same as `git status` reporting `?? .claude/` rather than every file
    /// inside it) instead of recursing into it file by file.
    /// `changed_files` must not choke on that directory entry, and must not
    /// report anything under `.claude/` at all.
    #[test]
    fn changed_files_skips_nested_git_worktree_directory() {
        let tmp = TempDir::new().expect("create tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);

        std::fs::create_dir_all(dir.join(".claude/worktrees")).unwrap();
        git(
            dir,
            &["worktree", "add", ".claude/worktrees/x", "-b", "wt-branch"],
        );

        let repo = Git2Repo::open(dir).expect("open fixture repo");
        let base_oid = repo.default_base_oid(Some("main")).expect("resolve base");
        let deltas = repo
            .changed_files(&base_oid)
            .expect("changed_files must not error on the nested worktree dir");

        assert!(
            deltas
                .iter()
                .all(|d| { !d.path.starts_with(".claude") && !d.path.starts_with(".git") }),
            ".claude/ (and .git) must be ignored entirely, got: {deltas:?}"
        );
    }

    /// Same repro as above, but through `build_graph`, matching the
    /// original bug report ("error building graph: ... Is a directory").
    #[test]
    fn build_graph_does_not_error_on_nested_git_worktree() {
        let tmp = TempDir::new().expect("create tempdir");
        let dir = tmp.path();
        git(dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);

        std::fs::create_dir_all(dir.join(".claude/worktrees")).unwrap();
        git(
            dir,
            &["worktree", "add", ".claude/worktrees/x", "-b", "wt-branch"],
        );

        let repo = Git2Repo::open(dir).expect("open fixture repo");
        let result = crate::pipeline::build_graph(
            &repo,
            &crate::pipeline::PipelineOptions {
                base_override: Some("main".to_string()),
            },
        );
        assert!(
            result.is_ok(),
            "build_graph must not abort on a nested git worktree: {result:?}"
        );
    }
}
