//! `Ctrl-e` on the file pane: lazygit-style alternate-screen handoff to a
//! real `nvim +<line> <file>` process, not an embedded `nvim --embed`
//! session. The GUI's embedded grid (see `crate::nvim`) is deliberately
//! out of scope for the TUI's phase 1 (issue #16): there's no
//! `ext_linegrid` rendering here, no RPC session kept alive across frames --
//! just "get out of the way, run the user's real editor, come back."
//!
//! [`suspend_and_run`] leaves the alternate screen and disables raw mode
//! (so `nvim` gets a normal terminal to draw into, exactly like `lazygit`
//! or `tig` handing off to `$EDITOR`), waits for the child to exit, then
//! restores both -- in that order even if `nvim` fails to spawn at all, so
//! a missing binary never leaves the terminal in a half-restored state for
//! the caller to clean up.
//!
//! # Working directory (issue #17's comment-flow fix)
//!
//! The spawned `nvim` is given `current_dir` = the repo root, not whatever
//! directory vdiff itself happened to be launched from. Without this, a
//! user running `vdiff --tui` from somewhere other than the repo root
//! (a worktree, a subdirectory, ...) would hand off to an `nvim` whose
//! *own* working directory disagreed with the repo `vdiff.nvim`'s
//! `:VdiffComment` flow resolves paths/`comments.json` against (see
//! `crate::review::store::comments_path`, which is keyed off the git dir) --
//! so a comment captured through this handoff could silently land against
//! the wrong repo, or `vdiff.nvim` could fail to find the repo at all. This
//! is exactly the bug the issue's companion fix calls out: the handoff
//! spawned bare `nvim +line <path>` with no working directory of its own,
//! inheriting whatever the shell that launched `vdiff` happened to be
//! sitting in.

use std::io;
use std::path::Path;
use std::process::Command;

use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;

/// Suspend the TUI, run `nvim +<line> <path>` (or just `nvim <path>` if
/// `line` is `None`) to completion with its working directory set to
/// `repo_root` (see the module doc), then resume. Returns an error if
/// either terminal-mode transition fails, or if `nvim` itself couldn't be
/// spawned (e.g. not on `PATH` -- callers should check
/// [`crate::nvim::session::nvim_available`] before offering this binding at
/// all, but a spawn failure here is still handled cleanly rather than
/// assumed impossible). A nonzero `nvim` exit status is not itself an
/// error -- the user may have `:cquit`ed or the file may not have existed;
/// either way there's nothing further for vdiff to do but resume.
pub fn suspend_and_run(path: &Path, line: Option<u32>, repo_root: &Path) -> io::Result<()> {
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    let mut command = Command::new("nvim");
    command.current_dir(repo_root);
    if let Some(line) = line {
        command.arg(format!("+{line}"));
    }
    command.arg(path);
    let spawn_result = command.status();

    // Restore the terminal unconditionally, even if `nvim` never ran --
    // otherwise a missing binary would leave the caller staring at a bare
    // shell prompt with raw mode off and no way back into the TUI short of
    // restarting it.
    io::stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;

    spawn_result.map(|_status| ())
}
