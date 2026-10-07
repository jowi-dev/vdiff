//! The impure half of issue #35's GitHub review threads: shelling out to
//! `gh` to find the PR and fetch its `reviewThreads` over GraphQL, reading
//! the local `HEAD` for the line-drift check, and running both on a
//! background thread so neither startup nor a refresh blocks a frontend.
//! Pairs with [`crate::review::gh_threads`] (the pure model) the same way
//! [`super::publish`] pairs with [`crate::review::publish`].
//!
//! `gh` stays the only credential holder, same design constraint as
//! [`super::pr`]'s module doc explains for `--pr`. Every failure here comes
//! back as a one-line, user-facing string rather than an error enum: the
//! only thing a frontend does with it is show it as a notice and carry on
//! without threads.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

use crate::review::gh_threads::{parse_threads_response, PrThreads, THREADS_QUERY};

/// Where to fetch threads from: a repo directory `gh` can resolve the
/// GitHub remote from, plus the PR number when it's known up front
/// (`--pr <n>`). `None` means "whatever PR the checked-out branch has".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSource {
    pub repo_path: PathBuf,
    pub pr: Option<u64>,
}

/// One finished fetch: the threads (or why there are none) plus the local
/// `HEAD` commit at fetch time, for [`crate::review::gh_threads::drift_warning`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    pub result: Result<PrThreads, String>,
    pub local_head: Option<String>,
}

/// Parse `gh pr view --json number`'s output.
pub fn parse_pr_number_json(json: &str) -> Result<u64, String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        number: u64,
    }
    serde_json::from_str::<Raw>(json)
        .map(|raw| raw.number)
        .map_err(|err| format!("couldn't parse `gh pr view` output: {err}"))
}

/// A fetch running on a background thread. Dropping it abandons the
/// result; the thread finishes its `gh` calls and exits on its own.
pub struct ThreadFetcher {
    rx: Receiver<FetchOutcome>,
}

impl ThreadFetcher {
    /// Fetch `source`'s threads on a new thread.
    pub fn spawn(source: ThreadSource) -> Self {
        Self::spawn_with(move || fetch(&source))
    }

    /// Run `work` on a new thread and hand its outcome back through
    /// [`Self::try_take`] -- the seam tests use to avoid shelling out.
    pub fn spawn_with(work: impl FnOnce() -> FetchOutcome + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // The receiver may be gone (a refresh replaced this fetcher);
            // nothing to do about that but drop the outcome.
            let _ = tx.send(work());
        });
        Self { rx }
    }

    /// The finished outcome, once, or `None` while the fetch is still
    /// running (or after it has already been taken).
    pub fn try_take(&self) -> Option<FetchOutcome> {
        self.rx.try_recv().ok()
    }
}

/// Fetch `source`'s PR threads and the local `HEAD`, blocking.
pub fn fetch(source: &ThreadSource) -> FetchOutcome {
    FetchOutcome {
        result: fetch_threads(source),
        local_head: local_head(&source.repo_path),
    }
}

fn fetch_threads(source: &ThreadSource) -> Result<PrThreads, String> {
    let number = match source.pr {
        Some(number) => number,
        None => parse_pr_number_json(&run_gh(
            &source.repo_path,
            &["pr", "view", "--json", "number"],
        )?)?,
    };
    let number_field = format!("number={number}");
    let query_field = format!("query={THREADS_QUERY}");
    let output = run_gh(
        &source.repo_path,
        &[
            "api",
            "graphql",
            "-F",
            "owner={owner}",
            "-F",
            "name={repo}",
            "-F",
            &number_field,
            "-f",
            &query_field,
        ],
    )?;
    parse_threads_response(&output)
}

/// `git rev-parse HEAD` in `repo_path`, or `None` if git can't say.
fn local_head(repo_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run `gh <args>` in `repo_path`, returning stdout or a one-line message.
fn run_gh(repo_path: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(repo_path)
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                "`gh` (GitHub CLI) not found on PATH".to_string()
            } else {
                format!("gh failed: {err}")
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("gh failed: {}", first.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn parse_pr_number_reads_number() {
        assert_eq!(parse_pr_number_json(r#"{"number": 35}"#), Ok(35));
    }

    #[test]
    fn parse_pr_number_rejects_garbage() {
        assert!(parse_pr_number_json("nope").is_err());
    }

    fn wait(fetcher: &ThreadFetcher) -> FetchOutcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(outcome) = fetcher.try_take() {
                return outcome;
            }
            assert!(Instant::now() < deadline, "fetch never finished");
            std::thread::yield_now();
        }
    }

    #[test]
    fn fetcher_hands_back_the_outcome_once() {
        let fetcher = ThreadFetcher::spawn_with(|| FetchOutcome {
            result: Err("no PR".to_string()),
            local_head: Some("abc".to_string()),
        });
        let outcome = wait(&fetcher);
        assert_eq!(outcome.result, Err("no PR".to_string()));
        assert_eq!(outcome.local_head.as_deref(), Some("abc"));
        assert!(fetcher.try_take().is_none());
    }

    #[test]
    fn fetch_outside_a_repo_reports_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = fetch(&ThreadSource {
            repo_path: dir.path().to_path_buf(),
            pr: None,
        });
        assert!(outcome.result.is_err());
        assert_eq!(outcome.local_head, None);
    }
}
