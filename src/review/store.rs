//! Glue-side IO wrapper around [`crate::review::comments`]: load/save the
//! local review-comment store at `<git_dir>/vdiff/comments.json`, where
//! `git_dir` is the repository's *actual* git directory (see
//! [`crate::pipeline::repo::GitRepo::git_dir`]) -- never `<repo
//! root>/.git` joined by hand, which breaks the moment `.git` is a gitlink
//! file rather than a directory (`git worktree add`, submodules; notably,
//! `vdiff`'s own review branches are developed inside exactly such a
//! worktree). Living under the git dir keeps comments out of `git
//! status`/diffs of the reviewed repo -- they're vdiff's own bookkeeping,
//! not part of the change set being reviewed -- and, as a bonus, gives each
//! worktree of a repo its own independent comment store rather than
//! sharing one. Pretty-printed and always re-saved in
//! [`crate::review::comments::sort_comments`] order, so a `git diff` of
//! `comments.json` itself (if anyone ever did track it) stays sane.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::comments::Comment;
use super::review_state::ReviewStore;

/// Where the comment store lives, given the repository's actual git
/// directory.
pub fn comments_path(git_dir: &Path) -> PathBuf {
    git_dir.join("vdiff").join("comments.json")
}

/// Load every stored comment, or an empty list if the store doesn't exist
/// yet (a fresh repo with no comments captured is not an error).
pub fn load(git_dir: &Path) -> io::Result<Vec<Comment>> {
    let path = comments_path(git_dir);
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

/// Save `comments` as pretty-printed JSON, creating `<git_dir>/vdiff/` if
/// it doesn't exist yet. Callers are expected to have already run
/// [`crate::review::comments::sort_comments`] (or gone through
/// [`crate::review::comments::add_comment`], which does it for them) --
/// this just serializes whatever order it's given.
pub fn save(git_dir: &Path, comments: &[Comment]) -> io::Result<()> {
    let path = comments_path(git_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(comments)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    fs::write(path, json)
}

/// Where the review-completion store lives, given the repository's actual
/// git directory -- see [`comments_path`]'s doc for why this has to be
/// [`crate::pipeline::repo::GitRepo::git_dir`], never a hand-joined
/// `<worktree>/.git`.
pub fn review_state_path(git_dir: &Path) -> PathBuf {
    git_dir.join("vdiff").join("review-state.json")
}

/// Load the review-completion store, or an empty [`ReviewStore`] if the
/// file doesn't exist yet, can't be read, or fails to parse. Unlike
/// [`load`] (which surfaces a corrupt `comments.json` as an error so a user
/// hand-editing it finds out), a busted `review-state.json` degrading to
/// "nothing reviewed yet" is the friendlier failure: this file is pure
/// bookkeeping vdiff itself writes on every toggle, never hand-edited, and
/// losing review progress to a parse error is a much smaller papercut than
/// refusing to start vdiff at all over it.
pub fn load_review_state(git_dir: &Path) -> ReviewStore {
    let path = review_state_path(git_dir);
    match fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => ReviewStore::default(),
    }
}

/// Save `store` as pretty-printed JSON, creating `<git_dir>/vdiff/` if it
/// doesn't exist yet -- the review-completion counterpart to [`save`].
pub fn save_review_state(git_dir: &Path, store: &ReviewStore) -> io::Result<()> {
    let path = review_state_path(git_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(store)
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

    fn sample_review_store() -> ReviewStore {
        use crate::review::review_state::{BranchReviewState, FileOid};
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        let mut nodes = BTreeMap::new();
        nodes.insert(
            "rust:crate::foo".to_string(),
            vec![FileOid {
                path: PathBuf::from("src/foo.rs"),
                oid: Some("abc123".to_string()),
            }],
        );
        let mut store = ReviewStore::default();
        store.set_branch("main", BranchReviewState { nodes });
        store
    }

    #[test]
    fn load_review_state_missing_file_returns_empty_store() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(load_review_state(tmp.path()), ReviewStore::default());
    }

    #[test]
    fn load_review_state_corrupt_file_returns_empty_store_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let path = review_state_path(tmp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not valid json").unwrap();
        assert_eq!(load_review_state(tmp.path()), ReviewStore::default());
    }

    #[test]
    fn save_review_state_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let store = sample_review_store();
        save_review_state(tmp.path(), &store).expect("save");
        assert_eq!(load_review_state(tmp.path()), store);
    }

    #[test]
    fn save_review_state_creates_git_vdiff_directory() {
        let tmp = TempDir::new().unwrap();
        save_review_state(tmp.path(), &sample_review_store()).expect("save");
        assert!(review_state_path(tmp.path()).exists());
    }

    #[test]
    fn review_state_and_comments_live_in_the_same_git_dir_but_different_files() {
        let tmp = TempDir::new().unwrap();
        assert_ne!(comments_path(tmp.path()), review_state_path(tmp.path()));
        assert_eq!(
            comments_path(tmp.path()).parent(),
            review_state_path(tmp.path()).parent()
        );
    }
}
