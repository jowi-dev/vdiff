//! `vdiff --pr <n>`: resolve a GitHub PR via `gh`, check its head ref out
//! into a disposable git worktree, and hand back the worktree path plus a
//! locally-diffable base ref -- so the rest of vdiff (`main`'s
//! `Git2Repo`/`build_graph`/GUI path) never has to know a PR was involved.
//!
//! Shelling out to `gh` (rather than talking to the GitHub API directly) is
//! a deliberate design constraint: it stays credential-free, reusing
//! whatever auth `gh` already has configured, and needs no API client of
//! our own. Both the PR-resolution and the actual git plumbing assume the
//! remote GitHub repo is checked out under the conventional `origin` name.
//!
//! `--pr` and `--base` compose by letting `--base` win: `--pr` supplies the
//! PR's own base ref as vdiff's default diff base, but an explicit `--base`
//! overrides it. Both are resolvable inside the temporary worktree either
//! way, since `git worktree add` shares the same object database as the
//! repo it's added from.
//!
//! Cleanup is best-effort: on normal exit, [`PrCheckout::cleanup_best_effort`]
//! removes the temporary worktree if it has no local modifications
//! (including untracked files), and otherwise leaves it in place with a
//! note on stderr. A panic or crash leaving a worktree behind under the
//! system temp dir is an acceptable failure mode -- it's inert until
//! something (a future `--pr` run's temp dir reuse, `git worktree prune`, a
//! `/tmp` reaper) cleans it up.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// Everything that can go wrong resolving and checking out `--pr <n>`.
#[derive(Debug, Error)]
pub enum PrError {
    /// `gh` isn't on `PATH`.
    #[error(
        "`gh` (GitHub CLI) not found on PATH -- install it from https://cli.github.com to use --pr"
    )]
    GhNotFound,
    /// `gh pr view` exited non-zero (not authed, PR not found, wrong repo,
    /// no `origin` remote, ...); `gh`'s own stderr already reads as a
    /// one-line, user-facing message.
    #[error("gh pr view failed: {0}")]
    GhFailed(String),
    /// `gh pr view`'s JSON output didn't parse or was missing a field.
    #[error("couldn't parse `gh pr view` output: {0}")]
    GhOutputInvalid(String),
    /// A `git` shellout (fetch/worktree add/worktree remove/status) failed.
    #[error("git {args} failed: {stderr}")]
    GitFailed { args: String, stderr: String },
}

/// The two refs `gh pr view <n> --json headRefName,baseRefName` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRefs {
    pub head_ref: String,
    pub base_ref: String,
}

/// Parse `gh pr view --json headRefName,baseRefName`'s output. Pure --
/// exercised directly by tests without shelling out to `gh` at all.
pub fn parse_pr_view_json(json: &str) -> Result<PrRefs, PrError> {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        #[serde(rename = "baseRefName")]
        base_ref_name: String,
    }
    let raw: Raw =
        serde_json::from_str(json).map_err(|err| PrError::GhOutputInvalid(err.to_string()))?;
    Ok(PrRefs {
        head_ref: raw.head_ref_name,
        base_ref: raw.base_ref_name,
    })
}

/// A unique-enough directory name under the system temp dir for PR
/// `pr_number`'s worktree: process id plus a nanosecond timestamp, so
/// repeated (even concurrent) `--pr` runs for the same PR number never
/// collide on a stale leftover directory.
fn worktree_dir_name(pr_number: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("vdiff-pr-{pr_number}-{}-{nanos}", std::process::id())
}

/// The ref to hand `--base`-shaped consumers for a fetched branch name:
/// `origin/<branch>`.
fn origin_tracking_ref(branch: &str) -> String {
    format!("origin/{branch}")
}

/// Run `gh pr view <n> --json headRefName,baseRefName` in `repo_path`.
fn gh_pr_view(repo_path: &Path, pr_number: u64) -> Result<PrRefs, PrError> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "headRefName,baseRefName",
        ])
        .current_dir(repo_path)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(PrError::GhNotFound),
        Err(err) => return Err(PrError::GhFailed(err.to_string())),
    };
    if !output.status.success() {
        return Err(PrError::GhFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    parse_pr_view_json(&String::from_utf8_lossy(&output.stdout))
}

/// Run `git <args>` in `dir`, returning trimmed stdout or a friendly error
/// naming the args and git's own stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String, PrError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|err| PrError::GitFailed {
            args: args.join(" "),
            stderr: err.to_string(),
        })?;
    if !output.status.success() {
        return Err(PrError::GitFailed {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Fetch `ref_spec` from `origin` and check it out, detached, into a fresh
/// worktree at `worktree_path`. Thin -- the actual git plumbing, factored
/// out so the lifecycle (add/status/remove) is testable against a local
/// repo and a local ref, without touching `gh` or a real GitHub remote.
fn add_worktree(repo_path: &Path, worktree_path: &Path, ref_spec: &str) -> Result<(), PrError> {
    git(repo_path, &["fetch", "origin", ref_spec])?;
    let worktree_path_str = worktree_path.to_string_lossy().into_owned();
    git(
        repo_path,
        &[
            "worktree",
            "add",
            "--detach",
            &worktree_path_str,
            "FETCH_HEAD",
        ],
    )?;
    Ok(())
}

/// Fetch `branch` from `origin` into its tracking ref (`origin/<branch>`),
/// creating/updating it explicitly via refspec rather than relying on the
/// remote's default fetch refspec already covering it.
fn fetch_tracking_ref(repo_path: &Path, branch: &str) -> Result<String, PrError> {
    let refspec = format!("{branch}:refs/remotes/origin/{branch}");
    git(repo_path, &["fetch", "origin", &refspec])?;
    Ok(origin_tracking_ref(branch))
}

/// `git status --porcelain` inside `worktree_path`: empty output means
/// clean (including untracked files, since porcelain reports those too).
fn worktree_status(worktree_path: &Path) -> Result<String, PrError> {
    git(worktree_path, &["status", "--porcelain"])
}

/// `git worktree remove <worktree_path>`, run from `repo_path` (removing a
/// worktree from within itself doesn't work, since that would delete the
/// process's own cwd).
fn remove_worktree(repo_path: &Path, worktree_path: &Path) -> Result<(), PrError> {
    let worktree_path_str = worktree_path.to_string_lossy().into_owned();
    git(repo_path, &["worktree", "remove", &worktree_path_str])?;
    Ok(())
}

/// Remove `worktree_path` (added from `repo_path`) if it has no local
/// modifications; otherwise leave it in place and print a note naming its
/// path. Never propagates an error -- a leftover temp worktree is an
/// acceptable failure mode, not worth failing the whole run over.
fn cleanup_worktree(repo_path: &Path, worktree_path: &Path) {
    match worktree_status(worktree_path) {
        Ok(status) if status.is_empty() => {
            if let Err(err) = remove_worktree(repo_path, worktree_path) {
                eprintln!(
                    "warning: failed to remove temporary PR worktree {} ({err})",
                    worktree_path.display()
                );
            }
        }
        Ok(_) => {
            eprintln!(
                "note: leaving modified PR worktree in place: {}",
                worktree_path.display()
            );
        }
        Err(err) => {
            eprintln!(
                "warning: couldn't check PR worktree for modifications ({err}); leaving it in place: {}",
                worktree_path.display()
            );
        }
    }
}

/// Resolve PR `pr_number`'s base branch via `gh pr view` (run in
/// `repo_path`) and fetch it into a local tracking ref, *without* checking
/// out the PR's head into a temporary worktree -- unlike [`PrCheckout`],
/// for callers (`vdiff --publish-comments`) that diff against the PR's
/// base but intend to operate on the caller's own current worktree, which
/// is assumed to already be the PR's head (give or take local edits).
/// Returns the resulting `origin/<base>` ref, suitable for
/// [`crate::pipeline::PipelineOptions::base_override`] or
/// [`crate::pipeline::repo::GitRepo::default_base_oid`].
pub fn resolve_pr_base_ref(repo_path: &Path, pr_number: u64) -> Result<String, PrError> {
    let refs = gh_pr_view(repo_path, pr_number)?;
    fetch_tracking_ref(repo_path, &refs.base_ref)
}

/// A checked-out PR: `worktree_path()` is a temporary, detached-HEAD
/// worktree at the PR's head ref; `base_ref()` is a locally-diffable ref
/// for the PR's base, suitable for [`crate::pipeline::PipelineOptions::base_override`].
pub struct PrCheckout {
    repo_path: PathBuf,
    worktree_path: PathBuf,
    base_ref: String,
}

impl PrCheckout {
    /// Resolve PR `pr_number` via `gh pr view` (run in `repo_path`), fetch
    /// its head into a fresh temporary worktree (never touching `repo_path`'s
    /// own checkout), and fetch its base branch so it's diffable locally.
    pub fn create(repo_path: &Path, pr_number: u64) -> Result<Self, PrError> {
        let refs = gh_pr_view(repo_path, pr_number)?;

        let worktree_path = std::env::temp_dir().join(worktree_dir_name(pr_number));
        add_worktree(repo_path, &worktree_path, &format!("pull/{pr_number}/head"))?;

        let base_ref = fetch_tracking_ref(repo_path, &refs.base_ref)?;

        Ok(Self {
            repo_path: repo_path.to_path_buf(),
            worktree_path,
            base_ref,
        })
    }

    /// The temporary worktree's path -- pass this as vdiff's effective
    /// `--repo`.
    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    /// The PR's base, as a locally-diffable ref -- use this as vdiff's
    /// effective `--base` unless the user gave their own.
    pub fn base_ref(&self) -> &str {
        &self.base_ref
    }

    /// Best-effort cleanup: see [`cleanup_worktree`].
    pub fn cleanup_best_effort(&self) {
        cleanup_worktree(&self.repo_path, &self.worktree_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn parse_pr_view_json_extracts_both_refs() {
        let json = r#"{"headRefName":"feature-branch","baseRefName":"main"}"#;
        let refs = parse_pr_view_json(json).unwrap();
        assert_eq!(refs.head_ref, "feature-branch");
        assert_eq!(refs.base_ref, "main");
    }

    #[test]
    fn parse_pr_view_json_rejects_missing_field() {
        let json = r#"{"headRefName":"feature-branch"}"#;
        assert!(matches!(
            parse_pr_view_json(json),
            Err(PrError::GhOutputInvalid(_))
        ));
    }

    #[test]
    fn parse_pr_view_json_rejects_invalid_json() {
        assert!(matches!(
            parse_pr_view_json("not json"),
            Err(PrError::GhOutputInvalid(_))
        ));
    }

    #[test]
    fn origin_tracking_ref_prefixes_with_origin() {
        assert_eq!(origin_tracking_ref("main"), "origin/main");
    }

    fn git_cmd(dir: &Path, args: &[&str]) {
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

    /// A bare "origin" remote plus a local clone, standing in for a real
    /// GitHub repo/checkout pair -- lets the worktree add/remove lifecycle
    /// be exercised against a local ref instead of a real `gh`/PR fetch.
    struct Fixture {
        _remote_dir: TempDir,
        clone_dir: TempDir,
    }

    fn fixture() -> Fixture {
        let remote_dir = TempDir::new().expect("create remote tempdir");
        git_cmd(remote_dir.path(), &["init", "--bare", "-b", "main"]);

        let seed_dir = TempDir::new().expect("create seed tempdir");
        git_cmd(seed_dir.path(), &["init", "-b", "main"]);
        std::fs::write(seed_dir.path().join("a.txt"), "hello\n").unwrap();
        git_cmd(seed_dir.path(), &["add", "."]);
        git_cmd(seed_dir.path(), &["commit", "-m", "initial"]);
        git_cmd(
            seed_dir.path(),
            &["push", remote_dir.path().to_str().unwrap(), "main:main"],
        );

        let clone_dir = TempDir::new().expect("create clone tempdir");
        git_cmd(
            clone_dir.path().parent().unwrap(),
            &[
                "clone",
                remote_dir.path().to_str().unwrap(),
                clone_dir.path().to_str().unwrap(),
            ],
        );

        Fixture {
            _remote_dir: remote_dir,
            clone_dir,
        }
    }

    #[test]
    fn add_worktree_checks_out_the_fetched_ref_detached() {
        let fx = fixture();
        let worktree_dir = TempDir::new().expect("create worktree tempdir");
        // Use the worktree tempdir's path but let `git worktree add` create
        // the leaf directory itself.
        let worktree_path = worktree_dir.path().join("checkout");

        add_worktree(fx.clone_dir.path(), &worktree_path, "main").unwrap();

        assert_eq!(
            std::fs::read_to_string(worktree_path.join("a.txt")).unwrap(),
            "hello\n"
        );

        // Clean up so the fixture's TempDirs can drop without leaving a
        // dangling worktree registration.
        cleanup_worktree(fx.clone_dir.path(), &worktree_path);
        assert!(!worktree_path.exists());
    }

    #[test]
    fn cleanup_worktree_removes_a_clean_worktree() {
        let fx = fixture();
        let worktree_dir = TempDir::new().expect("create worktree tempdir");
        let worktree_path = worktree_dir.path().join("checkout");
        add_worktree(fx.clone_dir.path(), &worktree_path, "main").unwrap();

        cleanup_worktree(fx.clone_dir.path(), &worktree_path);

        assert!(
            !worktree_path.exists(),
            "clean worktree should have been removed"
        );
    }

    #[test]
    fn cleanup_worktree_leaves_a_modified_worktree_in_place() {
        let fx = fixture();
        let worktree_dir = TempDir::new().expect("create worktree tempdir");
        let worktree_path = worktree_dir.path().join("checkout");
        add_worktree(fx.clone_dir.path(), &worktree_path, "main").unwrap();

        std::fs::write(worktree_path.join("a.txt"), "modified\n").unwrap();

        cleanup_worktree(fx.clone_dir.path(), &worktree_path);

        assert!(
            worktree_path.exists(),
            "modified worktree should have been left in place"
        );
        assert_eq!(
            std::fs::read_to_string(worktree_path.join("a.txt")).unwrap(),
            "modified\n"
        );

        // Force-clean so the test doesn't leak a worktree registration tied
        // to `fx.clone_dir` past this test.
        let worktree_path_str = worktree_path.to_string_lossy().into_owned();
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force", &worktree_path_str])
            .current_dir(fx.clone_dir.path())
            .status();
    }

    #[test]
    fn fetch_tracking_ref_creates_origin_tracking_branch() {
        let fx = fixture();
        let tracking_ref = fetch_tracking_ref(fx.clone_dir.path(), "main").unwrap();
        assert_eq!(tracking_ref, "origin/main");

        let rev = git(fx.clone_dir.path(), &["rev-parse", &tracking_ref]).unwrap();
        assert!(!rev.is_empty());
    }
}
