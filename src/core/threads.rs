//! The thread-list panel's state for GitHub PR review threads (issue #35),
//! held on [`crate::core::app::App::threads`] so both frontends drive it
//! through the same reducer. Everything GitHub-shaped (parsing, ordering,
//! formatting) lives in [`crate::review::gh_threads`]; this is only what
//! the panel and the graph badges need between frames.

use std::collections::HashMap;

use crate::graph::model::{NodeId, ProjectGraph};
use crate::review::gh_threads::{
    drift_warning, format_entry, list_entries, status_line, unresolved_counts, PrThreads,
    ThreadEntry,
};

/// Notice shown while a fetch is in flight.
pub const FETCHING_STATUS: &str = "fetching GitHub review threads...";

/// GitHub review threads as the panel and badges see them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThreadsState {
    /// The last successful fetch, or `None` before one has landed. A failed
    /// refresh keeps the previous data rather than blanking the panel.
    pub data: Option<PrThreads>,
    /// Unresolved thread count per node, for the graph badge.
    pub unresolved: HashMap<NodeId, usize>,
    /// The panel's rows, in [`list_entries`] order.
    pub entries: Vec<ThreadEntry>,
    /// Whether the thread list panel is open. While it is, it owns the
    /// keyboard: graph-pane messages are ignored.
    pub panel_open: bool,
    /// Index into `entries` of the highlighted row. Kept across closing
    /// and reopening the panel.
    pub selected: usize,
    /// One-line status for a notice: fetching, the thread summary, or why
    /// threads are unavailable. `None` before any fetch has started.
    pub status: Option<String>,
}

impl ThreadsState {
    /// `id`'s unresolved GitHub thread count, `0` when it has none.
    pub fn unresolved_for(&self, id: &NodeId) -> usize {
        self.unresolved.get(id).copied().unwrap_or(0)
    }

    /// The highlighted row, if there is one.
    pub fn selected_entry(&self) -> Option<ThreadEntry> {
        self.entries.get(self.selected).copied()
    }

    /// Fold one fetch outcome in: on success replace the data and recompute
    /// badges and rows against `graph`; on failure keep whatever was there
    /// and only report why.
    pub fn apply_fetch(
        &mut self,
        graph: &ProjectGraph,
        result: Result<PrThreads, String>,
        local_head: Option<&str>,
    ) {
        match result {
            Ok(threads) => {
                self.unresolved = unresolved_counts(graph, &threads.threads);
                self.entries = list_entries(&threads);
                self.selected = self.selected.min(self.entries.len().saturating_sub(1));
                let mut status = status_line(&threads);
                if let Some(warning) = drift_warning(&threads, local_head) {
                    status.push_str(" -- ");
                    status.push_str(&warning);
                }
                self.status = Some(status);
                self.data = Some(threads);
            }
            Err(message) => {
                self.status = Some(format!("GitHub threads unavailable: {message}"));
            }
        }
    }

    /// Each panel row's text, in `entries` order.
    pub fn row_labels(&self) -> Vec<String> {
        let Some(data) = &self.data else {
            return Vec::new();
        };
        self.entries
            .iter()
            .map(|&entry| format_entry(data, entry))
            .collect()
    }

    /// Move the highlight by `delta`, clamped to the rows.
    pub fn move_selection(&mut self, delta: i32) {
        let last = self.entries.len().saturating_sub(1) as i64;
        self.selected = (self.selected as i64 + i64::from(delta)).clamp(0, last) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::gh_threads::{ReviewThread, ThreadComment};

    #[test]
    fn row_labels_are_empty_before_any_threads_land() {
        assert!(ThreadsState::default().row_labels().is_empty());
    }

    #[test]
    fn row_labels_follow_entry_order() {
        let graph = ProjectGraph {
            nodes: HashMap::new(),
            roots: vec![],
            edges: vec![],
        };
        let thread = |path: &str, resolved: bool| ReviewThread {
            id: path.to_string(),
            path: path.to_string(),
            line: Some(1),
            is_resolved: resolved,
            is_outdated: false,
            comments: vec![ThreadComment {
                author: "a".to_string(),
                body: "b".to_string(),
            }],
        };
        let mut state = ThreadsState::default();
        state.apply_fetch(
            &graph,
            Ok(PrThreads {
                pr_number: 1,
                head_oid: "h".to_string(),
                threads: vec![thread("z.rs", true), thread("y.rs", false)],
                summaries: vec![],
            }),
            None,
        );
        assert_eq!(
            state.row_labels(),
            vec!["y.rs:1  @a  b", "z.rs:1  @a  b  [resolved]"]
        );
    }
}
