//! Diffing: line-level hunks ([`hunks`]) and word-level intraline
//! highlights within changed line pairs ([`intraline`]). Pure -- no I/O, no
//! egui/git2 dependency; the view layer (`ui::diff_view`) consumes these
//! types directly.

pub mod hunks;
pub mod intraline;
