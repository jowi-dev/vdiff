//! Glue-side IO wrapper around [`crate::review::comments`]: load/save the
//! local review-comment store at `<repo>/.git/vdiff/comments.json`. Kept
//! outside the `.git` worktree tracking (inside `.git` itself, like git's
//! own internal state) so comments never show up in `git status`/diffs of
//! the reviewed repo -- they're vdiff's own bookkeeping, not part of the
//! change set being reviewed. Pretty-printed and always re-saved in
//! [`crate::review::comments::sort_comments`] order, so a `git diff` of
//! `comments.json` itself (if anyone ever did track it) stays sane.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::comments::Comment;

/// Where the comment store lives for a repo whose git dir (or worktree
/// root -- either works, see the call sites) is `repo_root`.
pub fn comments_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".git").join("vdiff").join("comments.json")
}

/// Load every stored comment, or an empty list if the store doesn't exist
/// yet (a fresh repo with no comments captured is not an error).
pub fn load(repo_root: &Path) -> io::Result<Vec<Comment>> {
    let path = comments_path(repo_root);
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

/// Save `comments` as pretty-printed JSON, creating `.git/vdiff/` if it
/// doesn't exist yet. Callers are expected to have already run
/// [`crate::review::comments::sort_comments`] (or gone through
/// [`crate::review::comments::add_comment`], which does it for them) --
/// this just serializes whatever order it's given.
pub fn save(repo_root: &Path, comments: &[Comment]) -> io::Result<()> {
    let path = comments_path(repo_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(comments)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    fs::write(path, json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::comments::add_comment;
    use tempfile::TempDir;

    fn sample() -> Comment {
        Comment {
            id: "c1".to_string(),
            path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            text: "hello".to_string(),
            node: None,
            created_at: "2026-08-18T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn load_missing_store_returns_empty() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(load(tmp.path()).unwrap(), Vec::new());
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let mut comments = Vec::new();
        add_comment(&mut comments, sample());
        save(tmp.path(), &comments).expect("save");
        let loaded = load(tmp.path()).expect("load");
        assert_eq!(loaded, comments);
    }

    #[test]
    fn save_creates_git_vdiff_directory() {
        let tmp = TempDir::new().unwrap();
        save(tmp.path(), &[sample()]).expect("save");
        assert!(comments_path(tmp.path()).exists());
    }
}
