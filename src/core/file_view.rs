//! [`FileViewState`]: the file-viewer pane's loaded state -- the full head
//! (or, for deleted files, base) text of every file backing the node it was
//! opened for, which one is showing, and scroll position. Pure; scrolling/
//! jump/change-nav/file-switch transitions live here as small methods
//! [`crate::core::app::update`] calls, mirroring
//! [`crate::core::diff_state::DiffPaneState`]'s split for the full-screen
//! diff pane.

use std::path::PathBuf;

use crate::graph::model::NodeId;

/// One file's loaded content, backing one [`FileViewState::files`] entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileViewEntry {
    pub path: PathBuf,
    /// The file's lines, either head content or -- for a deleted file --
    /// base content (see [`Self::deleted`]).
    pub lines: Vec<String>,
    /// 0-based, inclusive head-line ranges covering every hunk that touches
    /// head content (see
    /// [`crate::pipeline::file_diff::changed_head_ranges`]). Empty for
    /// unchanged and deleted files.
    pub changed_ranges: Vec<(usize, usize)>,
    /// Whether this file is deleted at head -- `lines` holds its base
    /// content instead, and the renderer shows a "(deleted)" marker in the
    /// header rather than trying to line it up against nonexistent head
    /// content.
    pub deleted: bool,
}

/// The file-viewer pane's loaded state for the node it was opened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileViewState {
    /// The node the pane was opened for.
    pub node: NodeId,
    /// Every file backing that node, in the order
    /// [`crate::ui::eframe_app::DiffLoader`] loaded them.
    pub files: Vec<FileViewEntry>,
    /// Index into `files` of the file currently shown.
    pub file_index: usize,
    /// The topmost visible line, 0-based into the current file's `lines`.
    pub scroll_row: usize,
}

impl FileViewState {
    /// Build a fresh pane for `node`, showing its first file (if any) from
    /// the top.
    pub fn new(node: NodeId, files: Vec<FileViewEntry>) -> Self {
        Self {
            node,
            files,
            file_index: 0,
            scroll_row: 0,
        }
    }

    /// The file currently shown, or `None` if `files` is empty.
    pub fn current_file(&self) -> Option<&FileViewEntry> {
        self.files.get(self.file_index)
    }

    /// The number of lines in the current file, or 0 if `files` is empty.
    pub fn total_rows(&self) -> usize {
        self.current_file().map(|f| f.lines.len()).unwrap_or(0)
    }

    /// Shift `scroll_row` by `delta`, clamped to `[0, max_row]`. `max_row`
    /// is caller-supplied rather than derived from [`Self::total_rows`]
    /// internally -- [`crate::core::app::update`] computes it once (plain
    /// scroll clamps at `total_rows - 1`; half-page scroll may want a
    /// different cap) and this stays a pure clamp with no knowledge of
    /// which.
    pub fn scroll(&mut self, delta: i32, max_row: usize) {
        let shifted = self.scroll_row as i32 + delta;
        self.scroll_row = shifted.clamp(0, max_row as i32) as usize;
    }

    /// `gg`: jump to the top of the file.
    pub fn jump_top(&mut self) {
        self.scroll_row = 0;
    }

    /// `G`: jump to `total`'s last row (clamped to 0 if `total` is 0).
    pub fn jump_bottom(&mut self, total: usize) {
        self.scroll_row = total.saturating_sub(1);
    }

    /// `]c`: move `scroll_row` to the next changed range's start strictly
    /// after the current position. No wrap; a no-op past the last range or
    /// with no files.
    pub fn next_change(&mut self) {
        let Some(file) = self.current_file() else {
            return;
        };
        if let Some(&(start, _)) = file
            .changed_ranges
            .iter()
            .find(|&&(start, _)| start > self.scroll_row)
        {
            self.scroll_row = start;
        }
    }

    /// `[c`: move `scroll_row` to the previous changed range's start
    /// strictly before the current position. No wrap; a no-op before the
    /// first range or with no files.
    pub fn prev_change(&mut self) {
        let Some(file) = self.current_file() else {
            return;
        };
        if let Some(&(start, _)) = file
            .changed_ranges
            .iter()
            .rev()
            .find(|&&(start, _)| start < self.scroll_row)
        {
            self.scroll_row = start;
        }
    }

    /// Switch to the next/previous file, clamped to `files`'s bounds, and
    /// reset `scroll_row` to the top of the newly-shown file. A no-op if
    /// `files` is empty.
    pub fn shift_file(&mut self, delta: i32) {
        if self.files.is_empty() {
            return;
        }
        let max = self.files.len() as i32 - 1;
        let shifted = self.file_index as i32 + delta;
        self.file_index = shifted.clamp(0, max) as usize;
        self.scroll_row = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(lines: usize, changed_ranges: Vec<(usize, usize)>) -> FileViewEntry {
        FileViewEntry {
            path: PathBuf::from("f.rs"),
            lines: (0..lines).map(|i| i.to_string()).collect(),
            changed_ranges,
            deleted: false,
        }
    }

    fn state_with(files: Vec<FileViewEntry>) -> FileViewState {
        FileViewState::new(NodeId::from("n"), files)
    }

    #[test]
    fn total_rows_counts_current_file_lines() {
        let state = state_with(vec![entry(5, vec![])]);
        assert_eq!(state.total_rows(), 5);
    }

    #[test]
    fn total_rows_zero_with_no_files() {
        let state = state_with(vec![]);
        assert_eq!(state.total_rows(), 0);
    }

    #[test]
    fn scroll_clamps_to_given_max_row() {
        let mut state = state_with(vec![entry(10, vec![])]);
        state.scroll(-5, 9);
        assert_eq!(state.scroll_row, 0, "clamped at 0");
        state.scroll(20, 9);
        assert_eq!(state.scroll_row, 9, "clamped at max_row");
        state.scroll(-3, 9);
        assert_eq!(state.scroll_row, 6);
    }

    #[test]
    fn jump_top_resets_scroll_row() {
        let mut state = state_with(vec![entry(10, vec![])]);
        state.scroll_row = 7;
        state.jump_top();
        assert_eq!(state.scroll_row, 0);
    }

    #[test]
    fn jump_bottom_sets_scroll_row_to_last_row() {
        let mut state = state_with(vec![entry(10, vec![])]);
        state.jump_bottom(10);
        assert_eq!(state.scroll_row, 9);
    }

    #[test]
    fn jump_bottom_clamps_to_zero_for_empty_total() {
        let mut state = state_with(vec![entry(0, vec![])]);
        state.jump_bottom(0);
        assert_eq!(state.scroll_row, 0);
    }

    #[test]
    fn next_change_and_prev_change_move_between_range_starts() {
        let mut state = state_with(vec![entry(20, vec![(2, 3), (10, 12), (15, 15)])]);
        state.scroll_row = 0;
        state.next_change();
        assert_eq!(state.scroll_row, 2);
        state.next_change();
        assert_eq!(state.scroll_row, 10);
        state.next_change();
        assert_eq!(state.scroll_row, 15);
        state.next_change();
        assert_eq!(state.scroll_row, 15, "no wrap past the last range");

        state.prev_change();
        assert_eq!(state.scroll_row, 10);
        state.prev_change();
        assert_eq!(state.scroll_row, 2);
        state.prev_change();
        assert_eq!(state.scroll_row, 2, "no wrap before the first range");
    }

    #[test]
    fn next_change_noop_with_no_changed_ranges() {
        let mut state = state_with(vec![entry(20, vec![])]);
        state.next_change();
        assert_eq!(state.scroll_row, 0);
    }

    #[test]
    fn shift_file_clamps_and_resets_scroll() {
        let mut state = state_with(vec![entry(3, vec![]), entry(5, vec![]), entry(7, vec![])]);
        state.scroll_row = 2;

        state.shift_file(1);
        assert_eq!(state.file_index, 1);
        assert_eq!(state.scroll_row, 0, "reset on file switch");

        state.shift_file(10);
        assert_eq!(state.file_index, 2, "clamped at last file");

        state.shift_file(-10);
        assert_eq!(state.file_index, 0, "clamped at first file");
    }

    #[test]
    fn shift_file_noop_with_no_files() {
        let mut state = state_with(vec![]);
        state.shift_file(1);
        assert_eq!(state.file_index, 0);
    }
}
