//! Line-level diffing: [`diff_file`] runs `imara-diff` (Histogram algorithm)
//! over two whole-file strings and produces context-collapsed [`DiffHunk`]s
//! of [`LinePair`]s the diff pane renders directly. Pure -- no I/O, no
//! egui/git2 dependency.

use imara_diff::{Algorithm, Diff, InternedInput};

/// How many unchanged lines of context to keep on each side of a change
/// run. Two change runs separated by more than `2 * CONTEXT` unchanged
/// lines become separate hunks; closer than that, they merge into one.
const CONTEXT: usize = 3;

/// One line's relationship between the base and head versions of a file.
/// Indices are 0-based positions into [`FileDiff::base_lines`]/
/// [`FileDiff::head_lines`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinePair {
    /// Identical on both sides.
    Unchanged { base: u32, head: u32 },
    /// Present only at head.
    Added { head: u32 },
    /// Present only at base.
    Removed { base: u32 },
    /// A base line paired with a head line it was rewritten into --
    /// synthesized from equal-length runs of removed/added lines (see
    /// [`pair_lines`]); carries word-level highlights via
    /// [`crate::diffing::intraline::intraline`] at render time.
    Changed { base: u32, head: u32 },
}

/// A run of [`LinePair`]s: the changed lines plus up to [`CONTEXT`]
/// unchanged lines of surrounding context.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffHunk {
    pub lines: Vec<LinePair>,
}

/// A whole file's diff: every hunk, plus every line of both sides so the
/// view layer can render Unchanged/context rows and syntax-highlight
/// content by index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDiff {
    pub hunks: Vec<DiffHunk>,
    pub base_lines: Vec<String>,
    pub head_lines: Vec<String>,
}

/// Diff `base` against `head` line-by-line, returning context-collapsed
/// hunks. An empty `base` (whole-file add) yields one hunk of all
/// [`LinePair::Added`]; an empty `head` (whole-file delete), all
/// [`LinePair::Removed`]. Identical inputs yield no hunks.
pub fn diff_file(base: &str, head: &str) -> FileDiff {
    let base_lines = split_lines(base);
    let head_lines = head.lines().map(str::to_string).collect::<Vec<_>>();

    let input = InternedInput::new(base, head);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let all_lines = pair_lines(&diff, base_lines.len(), head_lines.len());
    let hunks = collapse_context(all_lines);

    FileDiff {
        hunks,
        base_lines,
        head_lines,
    }
}

/// Split `text` into lines, dropping any trailing newline (matches
/// `str::lines`'s semantics, kept as a named helper for symmetry with
/// `head_lines`'s construction in [`diff_file`]).
fn split_lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

/// Walk `diff`'s hunks alongside the implicit unchanged runs between them,
/// producing one [`LinePair`] per line across both `base_len` and
/// `head_len`. Within a hunk, removed/added runs of unequal length pair up
/// their first `min(removed, added)` lines as [`LinePair::Changed`],
/// leaving the remainder as plain [`LinePair::Removed`]/[`LinePair::Added`].
fn pair_lines(diff: &Diff, base_len: usize, head_len: usize) -> Vec<LinePair> {
    let mut result = Vec::new();
    let mut base_pos = 0usize;
    let mut head_pos = 0usize;

    for hunk in diff.hunks() {
        let before = hunk.before.start as usize..hunk.before.end as usize;
        let after = hunk.after.start as usize..hunk.after.end as usize;

        push_unchanged(&mut result, &mut base_pos, &mut head_pos, before.start);

        let removed = before.len();
        let added = after.len();
        let paired = removed.min(added);

        for _ in 0..paired {
            result.push(LinePair::Changed {
                base: base_pos as u32,
                head: head_pos as u32,
            });
            base_pos += 1;
            head_pos += 1;
        }
        for _ in paired..removed {
            result.push(LinePair::Removed {
                base: base_pos as u32,
            });
            base_pos += 1;
        }
        for _ in paired..added {
            result.push(LinePair::Added {
                head: head_pos as u32,
            });
            head_pos += 1;
        }
    }

    push_unchanged(&mut result, &mut base_pos, &mut head_pos, base_len);
    debug_assert_eq!(head_pos, head_len);

    result
}

/// Append [`LinePair::Unchanged`] entries until `base_pos` reaches
/// `target_base`, advancing `head_pos` in lockstep (the unchanged gap
/// between hunks is always the same length on both sides).
fn push_unchanged(
    result: &mut Vec<LinePair>,
    base_pos: &mut usize,
    head_pos: &mut usize,
    target_base: usize,
) {
    while *base_pos < target_base {
        result.push(LinePair::Unchanged {
            base: *base_pos as u32,
            head: *head_pos as u32,
        });
        *base_pos += 1;
        *head_pos += 1;
    }
}

/// Group `lines` into hunks: runs of non-[`LinePair::Unchanged`] lines
/// separated by more than `2 * CONTEXT` unchanged lines become separate
/// hunks, each padded with up to [`CONTEXT`] lines of context (clipped to
/// the file's bounds); closer runs merge into one hunk. No changes at all
/// yields no hunks.
fn collapse_context(lines: Vec<LinePair>) -> Vec<DiffHunk> {
    let change_idxs: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !matches!(line, LinePair::Unchanged { .. }))
        .map(|(i, _)| i)
        .collect();

    if change_idxs.is_empty() {
        return Vec::new();
    }

    let mut clusters: Vec<(usize, usize)> = Vec::new();
    let mut start = change_idxs[0];
    let mut end = change_idxs[0];
    for &idx in &change_idxs[1..] {
        if idx - end - 1 <= 2 * CONTEXT {
            end = idx;
        } else {
            clusters.push((start, end));
            start = idx;
            end = idx;
        }
    }
    clusters.push((start, end));

    clusters
        .into_iter()
        .map(|(start, end)| {
            let padded_start = start.saturating_sub(CONTEXT);
            let padded_end = (end + CONTEXT).min(lines.len() - 1);
            DiffHunk {
                lines: lines[padded_start..=padded_end].to_vec(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_files_have_no_hunks() {
        let diff = diff_file("a\nb\nc\n", "a\nb\nc\n");
        assert!(diff.hunks.is_empty());
        assert_eq!(diff.base_lines, vec!["a", "b", "c"]);
        assert_eq!(diff.head_lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn whole_file_added_is_one_all_added_hunk() {
        let diff = diff_file("", "x\ny\n");
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(
            diff.hunks[0].lines,
            vec![LinePair::Added { head: 0 }, LinePair::Added { head: 1 },]
        );
    }

    #[test]
    fn whole_file_deleted_is_one_all_removed_hunk() {
        let diff = diff_file("x\ny\n", "");
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(
            diff.hunks[0].lines,
            vec![LinePair::Removed { base: 0 }, LinePair::Removed { base: 1 },]
        );
    }

    /// Insert one line after `a`, delete `c`: base `a b c d`, head
    /// `a b x d`. Line 1 is unchanged (`b`), so it should show up as
    /// context around both the insertion and deletion rather than
    /// splitting them into separate hunks (only 1 unchanged line between
    /// the two change runs, well under the 6-line merge threshold).
    #[test]
    fn insert_and_delete_hunk_boundaries() {
        let diff = diff_file("a\nb\nc\nd\n", "a\nb\nx\nd\n");
        assert_eq!(diff.hunks.len(), 1, "close changes merge into one hunk");
        // c -> x is a same-length replace, so it's a single Changed pair,
        // not a separate Removed + Added.
        assert!(diff.hunks[0]
            .lines
            .iter()
            .any(|l| matches!(l, LinePair::Changed { .. })));
    }

    /// Two 1-line changes separated by 10 unchanged lines split into two
    /// hunks; each gets up to 3 lines of context on either side, so the
    /// hunks don't touch (context windows around each change are only 3
    /// lines deep, well short of the 10-line gap).
    #[test]
    fn distant_changes_split_into_separate_hunks() {
        let mut base_lines = vec!["x".to_string()];
        base_lines.extend((0..10).map(|i| format!("ctx{i}")));
        base_lines.push("y".to_string());
        let base = base_lines.join("\n") + "\n";

        let mut head_lines = vec!["X".to_string()];
        head_lines.extend((0..10).map(|i| format!("ctx{i}")));
        head_lines.push("Y".to_string());
        let head = head_lines.join("\n") + "\n";

        let diff = diff_file(&base, &head);
        assert_eq!(diff.hunks.len(), 2, "gap of 10 unchanged lines splits");
    }

    /// Changed-pair synthesis: 3 removed lines vs. 2 added lines pairs the
    /// first two as `Changed`, leaving the third removed line as a plain
    /// `Removed` (leftover stays Removed/Added per the spec).
    #[test]
    fn changed_pair_synthesis_leaves_leftover_on_the_longer_side() {
        let diff = diff_file("a\nr1\nr2\nr3\nz\n", "a\nn1\nn2\nz\n");
        let hunk = &diff.hunks[0];
        let changed: Vec<_> = hunk
            .lines
            .iter()
            .filter(|l| matches!(l, LinePair::Changed { .. }))
            .collect();
        let removed: Vec<_> = hunk
            .lines
            .iter()
            .filter(|l| matches!(l, LinePair::Removed { .. }))
            .collect();
        assert_eq!(changed.len(), 2, "two lines pair up");
        assert_eq!(removed.len(), 1, "the third removed line has no partner");
    }

    #[test]
    fn context_collapsing_keeps_up_to_three_lines_each_side() {
        // 5 lines of context before the change, 5 after; only 3 on each
        // side should make it into the hunk.
        let mut base = (0..5).map(|i| format!("ctx{i}")).collect::<Vec<_>>();
        base.push("old".to_string());
        base.extend((5..10).map(|i| format!("ctx{i}")));
        let base = base.join("\n") + "\n";

        let mut head = (0..5).map(|i| format!("ctx{i}")).collect::<Vec<_>>();
        head.push("new".to_string());
        head.extend((5..10).map(|i| format!("ctx{i}")));
        let head = head.join("\n") + "\n";

        let diff = diff_file(&base, &head);
        assert_eq!(diff.hunks.len(), 1);
        // 3 context + 1 change + 3 context = 7 lines.
        assert_eq!(diff.hunks[0].lines.len(), 7);
    }
}
