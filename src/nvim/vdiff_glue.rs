//! Frontend-neutral glue between an [`NvimSession`] and the
//! `:VdiffDiff`/`d` diffsplit-against-merge-base flow: registering the
//! `:VdiffDiff`/`:VdiffDiffOff` commands and the host-channel global,
//! reading the session's actual current buffer, resolving that (or a
//! caller-supplied fallback) down to a repo-relative path, and sending the
//! resulting [`NvimCmd::DiffSplit`]. None of this touches egui/eframe or
//! ratatui/crossterm -- both the GUI ([`crate::ui::nvim_pane`]) and the TUI
//! wrap these functions rather than duplicating them, which is also why
//! `session.call`'s timeout is threaded through as a parameter instead of
//! baked in here: each frontend picks its own UI-thread-blocking budget
//! (see [`crate::ui::nvim_pane::CALL_TIMEOUT`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmpv::Value;

use crate::nvim::session::{
    NvimCmd, NvimSession, HOST_CHANNEL_LUA, VDIFF_DIFF_COMMAND, VDIFF_DIFF_OFF_COMMAND,
};

/// Register `:VdiffDiff`/`:VdiffDiffOff` and set `vim.g.vdiff_host_channel`
/// in `session` -- fresh children (initial spawn, and every respawn) start
/// with no user commands or globals at all, so this has to run every time a
/// session comes up, not just once at startup. The host-channel global is
/// `vdiff.nvim`'s hook back into this embedder (see [`HOST_CHANNEL_LUA`]) --
/// comment capture itself (`:VdiffComment`, the compose UI, writing
/// `comments.json`) is that plugin's job, not this app's. Fire-and-forget
/// like [`NvimCmd::Ex`] generally -- a failure here (should never happen;
/// these are static, always-valid command strings) just means the command/
/// global won't exist this session, not a crash.
pub fn register_vdiff_commands(session: &NvimSession) {
    session.send(NvimCmd::Ex(VDIFF_DIFF_COMMAND.to_string()));
    session.send(NvimCmd::Ex(VDIFF_DIFF_OFF_COMMAND.to_string()));
    session.send(NvimCmd::ExecLua(HOST_CHANNEL_LUA.to_string()));
}

/// The current buffer's name, straight from nvim (`nvim_buf_get_name`) --
/// absolute for a real file, empty for an unnamed scratch buffer. `None` on
/// RPC failure (timeout/dead session), same as every other
/// [`NvimSession::call`]-based query. This is `:VdiffDiff`/`d`'s source of
/// truth for "what file is actually showing right now" -- see
/// [`resolve_diffed_path`] for turning this into a repo-relative path (or
/// `None` when the caller should fall back to its cached last-known file
/// instead).
pub fn current_buffer_name(session: &NvimSession, timeout: Duration) -> Option<String> {
    session
        .call("nvim_buf_get_name", vec![Value::from(0)], timeout)
        .and_then(|value| value.as_str().map(str::to_string))
}

/// Resolve the repo-relative path `:VdiffDiff`/`d` should diff, from nvim's
/// actual current-buffer name -- fixes a stale-diff bug where the glue
/// trusted whatever file it last opened via graph navigation even after the
/// user `:e`'d or `Ctrl-w w`'d to a different buffer inside nvim itself.
///
/// `buf_name` is [`current_buffer_name`]'s result verbatim: `None` if that
/// RPC call itself failed (timeout or a dead session -- same "couldn't get
/// an answer" as a boundary-detection query), `Some` with whatever
/// `nvim_buf_get_name(0)` returned otherwise (absolute for a real file,
/// empty for an unnamed buffer, `vdiff-base://<path>` for this plugin's own
/// diff-base scratch buffers).
///
/// Returns `None` -- meaning "the caller should fall back to its cached
/// last-known file instead, not treat this as an error" -- for:
/// - an RPC failure (`buf_name` itself `None`),
/// - an empty name (no buffer, or an unnamed scratch buffer),
/// - a `vdiff-base://` name (the user ran `:VdiffDiff` while focused in the
///   base split itself -- that's a request to refresh the file already
///   showing, not a new one to diff), or
/// - a name that doesn't resolve under `cwd` at all (edited via an
///   absolute path outside the repo -- there's no repo-relative
///   [`crate::graph::model::FileRef`] path for `base_blob` to look up).
///
/// Otherwise, `Some` of `name` stripped down to its path relative to `cwd`
/// -- what `GitRepo::base_blob` expects.
pub fn resolve_diffed_path(buf_name: Option<&str>, cwd: &Path) -> Option<PathBuf> {
    let name = buf_name?;
    if name.is_empty() || name.starts_with("vdiff-base://") {
        return None;
    }
    Path::new(name)
        .strip_prefix(cwd)
        .ok()
        .map(Path::to_path_buf)
}

/// Whether the current window is already at nvim's split boundary in
/// direction `dir` (`"h"` or `"l"`) -- i.e. `winnr()` and `winnr(dir)` agree
/// there's nowhere further to move. Used to decide whether a `Ctrl-w h`/
/// `Ctrl-w l` (or the arrow-key aliases) should hop out to the embedder's
/// own graph pane (at the boundary) or forward into nvim's own split
/// navigation (not at the boundary, nvim has internal splits to move
/// between). On *any* failure to get a straight answer -- timeout, RPC
/// error, dead session -- conservatively reports `true` ("at boundary"): a
/// wedged-but-not-dead nvim must never be able to trap keyboard focus, so
/// when in doubt, let the user out. Frontend-neutral (moved out of the GUI's
/// `crate::ui::nvim_pane::NvimPane`, which now wraps this) so the TUI can
/// reuse it for its own `Ctrl-w h`/`Ctrl-w l` chord.
pub fn at_boundary(session: &NvimSession, dir: &str, timeout: Duration) -> bool {
    let winnr = |args: Vec<Value>| {
        session.call(
            "nvim_call_function",
            vec![Value::from("winnr"), Value::Array(args)],
            timeout,
        )
    };
    match (winnr(vec![]), winnr(vec![Value::from(dir)])) {
        (Some(here), Some(there)) => here == there,
        _ => true,
    }
}

/// [`resolve_diffed_path`], falling back to `fallback` (the caller's
/// cached last-known file, e.g. the GUI's `nvim_current_file`) whenever
/// that resolves to `None`. Split out from [`trigger_diffsplit`] as its own
/// pure function so the fallback logic is unit-testable without a spawned
/// session.
fn resolve_diffed_path_with_fallback(
    buf_name: Option<&str>,
    cwd: &Path,
    fallback: Option<PathBuf>,
) -> Option<PathBuf> {
    resolve_diffed_path(buf_name, cwd).or(fallback)
}

/// The diffsplit-against-merge-base flow itself, shared by `:VdiffDiff`
/// (typed inside nvim) and any frontend's own "diff the focused node" key
/// binding (the GUI's nvim-mode `d`; a future TUI equivalent). Resolves
/// which file to diff from nvim's *actual* current buffer first --
/// [`current_buffer_name`] plus [`resolve_diffed_path`] -- rather than
/// trusting `fallback` blindly: that's meant to be written only when the
/// frontend's own navigation opened a file, so if the user `:e`'d or
/// `Ctrl-w w`'d to a different buffer inside nvim itself, trusting it
/// unconditionally would diff the file last opened that way, not the one
/// actually on screen -- a plausible-looking wrong diff. `fallback` is only
/// consulted when the RPC query can't produce a usable answer (timeout,
/// dead session), or the current buffer is unnamed/one of this plugin's
/// own `vdiff-base://` scratch buffers -- see [`resolve_diffed_path`]'s doc
/// for the full list.
///
/// `base_content_for` loads the resolved path's base content (typically
/// `GitRepo::base_blob` at whatever diff base the caller is holding) --
/// injected rather than hardcoded so this stays free of any I/O/repo
/// dependency; `None` (a missing base blob, e.g. an added file, or the
/// lookup itself erroring) is treated as empty content, matching the "diff
/// against an empty buffer" behavior that's correct and unsurprising for an
/// added file. Sends [`NvimCmd::DiffSplit`] and returns `true` if a path was
/// resolved at all; returns `false` (having sent nothing) if neither the
/// query nor `fallback` produced one -- the caller decides whether/how to
/// warn about that (shouldn't happen in practice, but a notification this
/// fires for can't assume the caller's state didn't move on by the time
/// it's processed).
pub fn trigger_diffsplit(
    session: &NvimSession,
    cwd: &Path,
    timeout: Duration,
    fallback: Option<PathBuf>,
    base_content_for: impl FnOnce(&Path) -> Option<String>,
) -> bool {
    let buf_name = current_buffer_name(session, timeout);
    let Some(path) = resolve_diffed_path_with_fallback(buf_name.as_deref(), cwd, fallback) else {
        return false;
    };
    let base_content = base_content_for(&path).unwrap_or_default();
    session.send(NvimCmd::DiffSplit { path, base_content });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_diffed_path_rpc_failure_falls_back() {
        assert_eq!(resolve_diffed_path(None, Path::new("/repo")), None);
    }

    #[test]
    fn resolve_diffed_path_empty_name_falls_back() {
        assert_eq!(resolve_diffed_path(Some(""), Path::new("/repo")), None);
    }

    #[test]
    fn resolve_diffed_path_base_scratch_buffer_falls_back() {
        assert_eq!(
            resolve_diffed_path(Some("vdiff-base://src/main.rs"), Path::new("/repo")),
            None
        );
    }

    #[test]
    fn resolve_diffed_path_strips_cwd_prefix() {
        assert_eq!(
            resolve_diffed_path(Some("/repo/src/main.rs"), Path::new("/repo")),
            Some(PathBuf::from("src/main.rs"))
        );
    }

    #[test]
    fn resolve_diffed_path_outside_repo_falls_back() {
        assert_eq!(
            resolve_diffed_path(Some("/elsewhere/other.rs"), Path::new("/repo")),
            None
        );
    }

    #[test]
    fn resolve_diffed_path_with_fallback_uses_resolved_path_when_available() {
        assert_eq!(
            resolve_diffed_path_with_fallback(
                Some("/repo/src/main.rs"),
                Path::new("/repo"),
                Some(PathBuf::from("stale.rs")),
            ),
            Some(PathBuf::from("src/main.rs"))
        );
    }

    #[test]
    fn resolve_diffed_path_with_fallback_falls_back_on_rpc_failure() {
        assert_eq!(
            resolve_diffed_path_with_fallback(
                None,
                Path::new("/repo"),
                Some(PathBuf::from("cached.rs")),
            ),
            Some(PathBuf::from("cached.rs"))
        );
    }

    #[test]
    fn resolve_diffed_path_with_fallback_falls_back_on_scratch_buffer() {
        assert_eq!(
            resolve_diffed_path_with_fallback(
                Some("vdiff-base://src/main.rs"),
                Path::new("/repo"),
                Some(PathBuf::from("cached.rs")),
            ),
            Some(PathBuf::from("cached.rs"))
        );
    }

    #[test]
    fn resolve_diffed_path_with_fallback_none_when_both_absent() {
        assert_eq!(
            resolve_diffed_path_with_fallback(None, Path::new("/repo"), None),
            None
        );
    }
}
