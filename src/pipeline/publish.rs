//! The impure half of `vdiff --publish-comments <pr>` (issue #7 phase 2):
//! shelling out to `gh` to learn `owner/repo` and to actually POST the
//! review, plus computing each commented file's changed head-line ranges
//! against the PR's base -- the input [`crate::review::publish::partition_comments`]
//! needs to decide which comments land as GitHub line comments. Pairs with
//! [`crate::review::publish`] (the pure logic) the same way [`super::pr`]
//! pairs `gh`/`git` shellouts with pure ref parsing.
//!
//! `gh` stays the only credential holder throughout: this module never
//! talks to the GitHub API directly, same design constraint as
//! [`super::pr`]'s module doc explains for `--pr`.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::diffing::hunks::diff_file;
use crate::pipeline::error::Result as PipelineResult;
use crate::pipeline::file_diff::changed_head_ranges;
use crate::pipeline::repo::GitRepo;

/// Everything that can go wrong resolving `owner/repo` or posting a review
/// via `gh`.
#[derive(Debug, Error)]
pub enum PublishGhError {
    /// `gh` isn't on `PATH`.
    #[error(
        "`gh` (GitHub CLI) not found on PATH -- install it from https://cli.github.com to use --publish-comments"
    )]
    GhNotFound,
    /// A `gh` invocation exited non-zero; `gh`'s own stderr already reads
    /// as a one-line, user-facing message.
    #[error("gh failed: {0}")]
    GhFailed(String),
    /// `gh repo view`'s JSON output didn't parse or was missing a field.
    #[error("couldn't parse `gh repo view` output: {0}")]
    GhOutputInvalid(String),
}

/// Run `gh repo view --json nameWithOwner` in `repo_path` and return
/// `owner/repo`, so the caller doesn't have to know or guess it -- the
/// review-posting endpoint needs it in the URL.
pub fn repo_name_with_owner(repo_path: &Path) -> Result<String, PublishGhError> {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }
    let output = run_gh(repo_path, &["repo", "view", "--json", "nameWithOwner"])?;
    let raw: Raw = serde_json::from_str(&output)
        .map_err(|err| PublishGhError::GhOutputInvalid(err.to_string()))?;
    Ok(raw.name_with_owner)
}

/// POST `payload_json` (the body [`crate::review::publish::build_payload`]
/// produced, already serialized) to `repos/{owner_repo}/pulls/{pr_number}/reviews`
/// via `gh api ... --input -`, feeding it over stdin rather than a temp
/// file. `gh`'s exit status is the sole success signal: a non-zero exit
/// means the whole review failed to post (nothing was partially applied on
/// GitHub's side for a single `POST`), surfaced as `gh`'s own stderr.
pub fn post_review(
    repo_path: &Path,
    owner_repo: &str,
    pr_number: u64,
    payload_json: &str,
) -> Result<(), PublishGhError> {
    let endpoint = format!("repos/{owner_repo}/pulls/{pr_number}/reviews");
    let mut child = Command::new("gh")
        .args(["api", &endpoint, "--input", "-"])
        .current_dir(repo_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                PublishGhError::GhNotFound
            } else {
                PublishGhError::GhFailed(err.to_string())
            }
        })?;
    child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(payload_json.as_bytes())
        .map_err(|err| PublishGhError::GhFailed(err.to_string()))?;
    let output = child
        .wait_with_output()
        .map_err(|err| PublishGhError::GhFailed(err.to_string()))?;
    if !output.status.success() {
        return Err(PublishGhError::GhFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// Run `gh <args>` in `repo_path`, returning trimmed stdout or a
/// [`PublishGhError`] naming `gh`'s own stderr -- the read-only counterpart
/// to [`post_review`]'s spawn/stdin/wait dance.
fn run_gh(repo_path: &Path, args: &[&str]) -> Result<String, PublishGhError> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(repo_path)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(PublishGhError::GhNotFound)
        }
        Err(err) => return Err(PublishGhError::GhFailed(err.to_string())),
    };
    if !output.status.success() {
        return Err(PublishGhError::GhFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Changed head-line ranges (1-based, inclusive -- matching
/// [`crate::review::comments::Comment::start_line`]'s convention) for every
/// path in `paths`, diffed against `base_oid`. Only computes for the given
/// paths (the ones with at least one local comment) rather than the whole
/// change set, since `--publish-comments` never needs a full graph build.
/// A path absent from the change set entirely still gets an entry with no
/// ranges (base and head content are identical, so [`diff_file`] finds no
/// hunks) -- callers don't need to special-case "untouched file" separately
/// from "touched file, but this exact range wasn't changed".
pub fn changed_ranges_for_paths(
    repo: &dyn GitRepo,
    base_oid: &str,
    paths: &HashSet<String>,
) -> PipelineResult<HashMap<String, Vec<(u32, u32)>>> {
    let mut out = HashMap::with_capacity(paths.len());
    for path in paths {
        let path_buf = std::path::PathBuf::from(path);
        let base_content = repo.base_blob(base_oid, &path_buf)?.unwrap_or_default();
        let head_content = repo.head_content(&path_buf)?.unwrap_or_default();
        let diff = diff_file(&base_content, &head_content);
        let ranges = changed_head_ranges(&diff)
            .into_iter()
            .map(|(lo, hi)| (lo as u32 + 1, hi as u32 + 1))
            .collect();
        out.insert(path.clone(), ranges);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::repo::FakeRepo;
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;

    fn repo() -> FakeRepo {
        let mut base_files = StdHashMap::new();
        base_files.insert(
            PathBuf::from("src/lib.rs"),
            "line one\nline two\nline three\n".to_string(),
        );
        let mut head_files = StdHashMap::new();
        head_files.insert(
            PathBuf::from("src/lib.rs"),
            "line one\nCHANGED\nline three\n".to_string(),
        );
        head_files.insert(PathBuf::from("src/untouched.rs"), "same\n".to_string());
        base_files.insert(PathBuf::from("src/untouched.rs"), "same\n".to_string());
        FakeRepo {
            default_base_oid: "base-oid".to_string(),
            base_files,
            head_files,
            ..Default::default()
        }
    }

    #[test]
    fn changed_ranges_for_paths_reports_ranges_only_for_changed_lines() {
        let repo = repo();
        let mut paths = HashSet::new();
        paths.insert("src/lib.rs".to_string());

        let ranges = changed_ranges_for_paths(&repo, "base-oid", &paths).unwrap();

        // 0-based line index 1 ("line two" -> "CHANGED") becomes 1-based
        // line 2.
        assert_eq!(ranges["src/lib.rs"], vec![(2, 2)]);
    }

    #[test]
    fn changed_ranges_for_paths_untouched_file_has_no_ranges() {
        let repo = repo();
        let mut paths = HashSet::new();
        paths.insert("src/untouched.rs".to_string());

        let ranges = changed_ranges_for_paths(&repo, "base-oid", &paths).unwrap();

        assert_eq!(ranges["src/untouched.rs"], Vec::<(u32, u32)>::new());
    }
}
