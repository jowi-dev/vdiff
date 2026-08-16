//! [`DiffPaneState`]: the diff pane's loaded state -- which files back the
//! node it was opened on, which one is showing, scroll position, and side-
//! by-side/unified mode. Pure; scrolling/hunk-jump/file-switch transitions
//! live here as small methods [`crate::core::app::update`] calls, keeping
//! `app.rs` from having to know about rows/hunks directly.

use std::path::PathBuf;

use crate::diffing::hunks::FileDiff;
use crate::graph::model::NodeId;

/// Side-by-side (base | head columns) or unified (single column, +/-/space
/// gutter) diff rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    SideBySide,
    Unified,
}

/// One file's loaded diff, backing one [`DiffPaneState::files`] entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub diff: FileDiff,
}

/// The diff pane's loaded state for the node it was opened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPaneState {
    /// The node the pane was opened for.
    pub node: NodeId,
    /// Every file backing that node, in the order [`crate::ui::eframe_app`]
    /// loaded them.
    pub files: Vec<FileEntry>,
    /// Index into `files` of the file currently shown.
    pub file_index: usize,
    /// The topmost rendered row currently scrolled to, within the current
    /// file's rendered rows (see [`Self::total_rows`]).
    pub scroll_row: usize,
    /// The current rendering mode.
    pub mode: DiffMode,
}

impl DiffPaneState {
    /// Build a fresh pane for `node`, showing its first file (if any) from
    /// the top in side-by-side mode.
    pub fn new(node: NodeId, files: Vec<FileEntry>) -> Self {
        Self {
            node,
            files,
            file_index: 0,
            scroll_row: 0,
            mode: DiffMode::SideBySide,
        }
    }

    /// The file currently shown, or `None` if `files` is empty.
    pub fn current_file(&self) -> Option<&FileEntry> {
        self.files.get(self.file_index)
    }

    /// The number of rendered rows in the current file: every line across
    /// every hunk. Collapsed context between hunks isn't rendered, so it
    /// isn't counted.
    pub fn total_rows(&self) -> usize {
        self.current_file()
            .map(|file| file.diff.hunks.iter().map(|hunk| hunk.lines.len()).sum())
            .unwrap_or(0)
    }

    /// The row index (into the current file's concatenated rendered rows)
    /// each of its hunks starts at, in order.
    pub fn hunk_start_rows(&self) -> Vec<usize> {
        let Some(file) = self.current_file() else {
            return Vec::new();
        };
        let mut rows = Vec::with_capacity(file.diff.hunks.len());
        let mut row = 0;
        for hunk in &file.diff.hunks {
            rows.push(row);
            row += hunk.lines.len();
        }
        rows
    }

    /// Shift `scroll_row` by `delta`, clamped to
    /// `[0, total_rows.saturating_sub(1)]`.
    pub fn scroll(&mut self, delta: i32) {
        let max = self.total_rows().saturating_sub(1) as i32;
        let shifted = self.scroll_row as i32 + delta;
        self.scroll_row = shifted.clamp(0, max) as usize;
    }

    /// Jump `scroll_row` to the first hunk start strictly after the
    /// current position. A no-op if already at or past the last hunk.
    pub fn next_hunk(&mut self) {
        if let Some(&row) = self
            .hunk_start_rows()
            .iter()
            .find(|&&row| row > self.scroll_row)
        {
            self.scroll_row = row;
        }
    }

    /// Jump `scroll_row` to the last hunk start strictly before the
    /// current position. A no-op if already at or before the first hunk.
    pub fn prev_hunk(&mut self) {
        if let Some(&row) = self
            .hunk_start_rows()
            .iter()
            .rev()
            .find(|&&row| row < self.scroll_row)
        {
            self.scroll_row = row;
        }
    }

    /// Flip between side-by-side and unified rendering.
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            DiffMode::SideBySide => DiffMode::Unified,
            DiffMode::Unified => DiffMode::SideBySide,
        };
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
    use crate::diffing::hunks::{DiffHunk, LinePair};

    fn hunk_of(len: usize) -> DiffHunk {
        DiffHunk {
            lines: (0..len)
                .map(|i| LinePair::Unchanged {
                    base: i as u32,
                    head: i as u32,
                })
                .collect(),
        }
    }

    fn file(hunks: Vec<DiffHunk>) -> FileEntry {
        FileEntry {
            path: PathBuf::from("f.rs"),
            diff: FileDiff {
                hunks,
                base_lines: vec![],
                head_lines: vec![],
            },
        }
    }

    fn state_with(files: Vec<FileEntry>) -> DiffPaneState {
        DiffPaneState::new(NodeId::from("n"), files)
    }

    #[test]
    fn total_rows_sums_all_hunk_lines() {
        let state = state_with(vec![file(vec![hunk_of(3), hunk_of(5)])]);
        assert_eq!(state.total_rows(), 8);
    }

    #[test]
    fn total_rows_zero_with_no_files() {
        let state = state_with(vec![]);
        assert_eq!(state.total_rows(), 0);
    }

    #[test]
    fn scroll_clamps_to_bounds() {
        let mut state = state_with(vec![file(vec![hunk_of(3)])]);
        state.scroll(-5);
        assert_eq!(state.scroll_row, 0, "clamped at 0");
        state.scroll(10);
        assert_eq!(state.scroll_row, 2, "clamped at total_rows - 1");
        state.scroll(-1);
        assert_eq!(state.scroll_row, 1);
    }

    #[test]
    fn scroll_noop_range_with_no_rows() {
        let mut state = state_with(vec![]);
        state.scroll(5);
        assert_eq!(state.scroll_row, 0);
    }

    #[test]
    fn hunk_start_rows_are_cumulative_offsets() {
        let state = state_with(vec![file(vec![hunk_of(3), hunk_of(2), hunk_of(4)])]);
        assert_eq!(state.hunk_start_rows(), vec![0, 3, 5]);
    }

    #[test]
    fn next_hunk_jumps_to_first_start_after_current_row() {
        let mut state = state_with(vec![file(vec![hunk_of(3), hunk_of(2), hunk_of(4)])]);
        state.scroll_row = 1;
        state.next_hunk();
        assert_eq!(state.scroll_row, 3);
        state.next_hunk();
        assert_eq!(state.scroll_row, 5);
        state.next_hunk();
        assert_eq!(state.scroll_row, 5, "no-op past the last hunk");
    }

    #[test]
    fn prev_hunk_jumps_to_last_start_before_current_row() {
        let mut state = state_with(vec![file(vec![hunk_of(3), hunk_of(2), hunk_of(4)])]);
        state.scroll_row = 5;
        state.prev_hunk();
        assert_eq!(state.scroll_row, 3);
        state.prev_hunk();
        assert_eq!(state.scroll_row, 0);
        state.prev_hunk();
        assert_eq!(state.scroll_row, 0, "no-op at the first hunk");
    }

    #[test]
    fn toggle_mode_flips_between_side_by_side_and_unified() {
        let mut state = state_with(vec![file(vec![hunk_of(1)])]);
        assert_eq!(state.mode, DiffMode::SideBySide);
        state.toggle_mode();
        assert_eq!(state.mode, DiffMode::Unified);
        state.toggle_mode();
        assert_eq!(state.mode, DiffMode::SideBySide);
    }

    #[test]
    fn shift_file_clamps_and_resets_scroll() {
        let mut state = state_with(vec![
            file(vec![hunk_of(1)]),
            file(vec![hunk_of(2)]),
            file(vec![hunk_of(3)]),
        ]);
        state.scroll_row = 0;
        state.scroll(0);

        state.shift_file(1);
        assert_eq!(state.file_index, 1);
        state.scroll(5);
        assert_eq!(state.scroll_row, 1, "clamped to file 1's 2-row max");

        state.shift_file(1);
        assert_eq!(state.file_index, 2);
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
