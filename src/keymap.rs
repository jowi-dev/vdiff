//! Pure `KeyInput -> Msg` mapping, independent of any GUI toolkit's key
//! event type. `map_key` never touches `App` state directly; the caller
//! threads `KeyContext` in (current screen, pane, whether a picker/file pane
//! is open) and carries `pending` across calls to implement the
//! two-keystroke `gd`/`gr`/`gg` (graph pane/file pane), `]c`/`[c`/`]f`/`[f`
//! (diff pane and file pane), and `Ctrl-w h`/`Ctrl-w l` (pane switch)
//! chords.

use crate::core::app::{Msg, Pane, Screen};
use crate::core::focus::Direction;

/// A single keypress, abstracted away from any GUI toolkit's key event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInput {
    /// A printable character key, case-sensitive (`Char('g')` vs.
    /// `Char('G')` are distinct -- the latter is Shift-G).
    Char(char),
    /// A character key held with Ctrl (`Ctrl('w')` is Ctrl-W).
    Ctrl(char),
    /// The Enter/Return key.
    Enter,
    /// The Escape key.
    Esc,
    /// An arrow key, reusing [`Direction`] rather than a dedicated enum --
    /// only meaningful today as the second half of the `Ctrl-w` pane-switch
    /// chord (`Ctrl-w Right`/`Ctrl-w Left` alias `Ctrl-w l`/`Ctrl-w h`, see
    /// [`resolve_pending`]); unbound everywhere else (arrows outside a
    /// chord fall through every match arm to [`KeyOutcome::None`]).
    Arrow(Direction),
}

/// A prefix key remembered across [`map_key`] calls to complete a chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// A plain character prefix: `g` (graph pane's `gd`/`gr`, file pane's
    /// `gg`), `]`/`[` (diff pane's and file pane's hunk/change/file jumps).
    Char(char),
    /// `Ctrl-w` -- completed by `h`/`l` into [`Msg::PaneLeft`]/
    /// [`Msg::PaneRight`].
    CtrlW,
}

/// Everything [`map_key`] needs besides the keypress itself: where in the
/// app the key landed, and any pending prefix key from the previous call
/// (see [`KeyOutcome::Pending`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyContext {
    /// The screen currently shown.
    pub screen: Screen,
    /// Which panel has keyboard focus on [`Screen::Graph`]. Ignored on
    /// [`Screen::Diff`].
    pub pane: Pane,
    /// Whether the file viewer pane is currently open -- gates `Ctrl-w l`
    /// (there's nothing to switch focus to if it isn't).
    pub file_open: bool,
    /// Whether the edge-following picker overlay is open. Checked ahead of
    /// `screen`/`pane` -- the picker only ever opens over
    /// [`Screen::Graph`]/[`Pane::Graph`], but its keys take priority
    /// regardless.
    pub picker_open: bool,
    /// A prefix key returned as [`KeyOutcome::Pending`] by the previous
    /// call, or `None` if no chord is in progress.
    pub pending: Option<Pending>,
}

/// The result of [`map_key`]: either a [`Msg`] to dispatch, a prefix to
/// remember and pass back in as `KeyContext::pending` on the next
/// keypress, or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Dispatch this message.
    Msg(Msg),
    /// `key` started a chord; remember it and pass it back in as `pending`
    /// on the next keypress.
    Pending(Pending),
    /// No mapping for this key in this context.
    None,
}

/// Map a keypress to a [`KeyOutcome`], per `ctx`.
///
/// Precedence:
/// 1. `ctx.picker_open` -- `j`/`k` move the selection, `Enter` selects,
///    `Esc` cancels; everything else is unmapped.
/// 2. `ctx.pending` set -- completes a chord started by a previous call
///    (see [`resolve_pending`]); any other completion clears the chord
///    with no message.
/// 3. Otherwise, per `ctx.screen`/`ctx.pane`:
///    - [`Screen::Graph`]/[`Pane::Graph`]: `h`/`j`/`k`/`l` ->
///      [`Msg::FocusMove`], `Enter` -> [`Msg::OpenFile`], `d` ->
///      [`Msg::OpenDiff`], `g` -> [`KeyOutcome::Pending`], `t` ->
///      [`Msg::ToggleTests`], `Ctrl-w` -> [`KeyOutcome::Pending`].
///    - [`Screen::Graph`]/[`Pane::File`]: `j`/`k` -> [`Msg::FileScroll`],
///      `Ctrl-d`/`Ctrl-u` -> [`Msg::FileHalfPage`], `g`/`]`/`[` ->
///      [`KeyOutcome::Pending`], `G` -> [`Msg::FileJumpBottom`], `d` ->
///      [`Msg::OpenDiff`], `Esc` -> [`Msg::CloseFile`], `Ctrl-w` ->
///      [`KeyOutcome::Pending`].
///    - [`Screen::Diff`]: `Esc` -> [`Msg::CloseDiff`], `j`/`k` ->
///      [`Msg::DiffScroll`], `s` -> [`Msg::DiffToggleMode`], `]`/`[` ->
///      [`KeyOutcome::Pending`].
///
/// [`update`]: crate::core::app::update
pub fn map_key(key: KeyInput, ctx: KeyContext) -> KeyOutcome {
    if ctx.picker_open {
        return match key {
            KeyInput::Char('j') => KeyOutcome::Msg(Msg::PickerMove(1)),
            KeyInput::Char('k') => KeyOutcome::Msg(Msg::PickerMove(-1)),
            KeyInput::Enter => KeyOutcome::Msg(Msg::PickerSelect),
            KeyInput::Esc => KeyOutcome::Msg(Msg::PickerCancel),
            _ => KeyOutcome::None,
        };
    }

    if let Some(pending) = ctx.pending {
        return resolve_pending(pending, key, ctx);
    }

    match ctx.screen {
        Screen::Graph => match ctx.pane {
            Pane::Graph => match key {
                KeyInput::Char('h') => KeyOutcome::Msg(Msg::FocusMove(Direction::Left)),
                KeyInput::Char('j') => KeyOutcome::Msg(Msg::FocusMove(Direction::Down)),
                KeyInput::Char('k') => KeyOutcome::Msg(Msg::FocusMove(Direction::Up)),
                KeyInput::Char('l') => KeyOutcome::Msg(Msg::FocusMove(Direction::Right)),
                KeyInput::Enter => KeyOutcome::Msg(Msg::OpenFile),
                KeyInput::Char('d') => KeyOutcome::Msg(Msg::OpenDiff),
                KeyInput::Char('g') => KeyOutcome::Pending(Pending::Char('g')),
                KeyInput::Char('t') => KeyOutcome::Msg(Msg::ToggleTests),
                KeyInput::Ctrl('w') => KeyOutcome::Pending(Pending::CtrlW),
                _ => KeyOutcome::None,
            },
            Pane::File => match key {
                KeyInput::Char('j') => KeyOutcome::Msg(Msg::FileScroll(1)),
                KeyInput::Char('k') => KeyOutcome::Msg(Msg::FileScroll(-1)),
                KeyInput::Ctrl('d') => KeyOutcome::Msg(Msg::FileHalfPage(1)),
                KeyInput::Ctrl('u') => KeyOutcome::Msg(Msg::FileHalfPage(-1)),
                KeyInput::Char('g') => KeyOutcome::Pending(Pending::Char('g')),
                KeyInput::Char('G') => KeyOutcome::Msg(Msg::FileJumpBottom),
                KeyInput::Char(']') => KeyOutcome::Pending(Pending::Char(']')),
                KeyInput::Char('[') => KeyOutcome::Pending(Pending::Char('[')),
                KeyInput::Char('d') => KeyOutcome::Msg(Msg::OpenDiff),
                KeyInput::Esc => KeyOutcome::Msg(Msg::CloseFile),
                KeyInput::Ctrl('w') => KeyOutcome::Pending(Pending::CtrlW),
                _ => KeyOutcome::None,
            },
        },
        Screen::Diff => match key {
            KeyInput::Esc => KeyOutcome::Msg(Msg::CloseDiff),
            KeyInput::Char('j') => KeyOutcome::Msg(Msg::DiffScroll(1)),
            KeyInput::Char('k') => KeyOutcome::Msg(Msg::DiffScroll(-1)),
            KeyInput::Char('s') => KeyOutcome::Msg(Msg::DiffToggleMode),
            KeyInput::Char(']') => KeyOutcome::Pending(Pending::Char(']')),
            KeyInput::Char('[') => KeyOutcome::Pending(Pending::Char('[')),
            _ => KeyOutcome::None,
        },
    }
}

/// Complete a chord: `pending` (from the previous call) plus this call's
/// `key`, in `ctx`'s screen/pane. Unlike the top-level dispatch in
/// [`map_key`], this checks `ctx.screen`/`ctx.pane` explicitly because the
/// same prefix character means different things in different panes (`g` is
/// `gd`/`gr` on [`Pane::Graph`] but `gg` on [`Pane::File`]; `]`/`[` are
/// hunk/file jumps on [`Screen::Diff`] but change/file jumps on
/// [`Pane::File`]).
fn resolve_pending(pending: Pending, key: KeyInput, ctx: KeyContext) -> KeyOutcome {
    match (pending, ctx.screen, ctx.pane, key) {
        (Pending::Char('g'), Screen::Graph, Pane::Graph, KeyInput::Char('d')) => {
            KeyOutcome::Msg(Msg::FollowDeps)
        }
        (Pending::Char('g'), Screen::Graph, Pane::Graph, KeyInput::Char('r')) => {
            KeyOutcome::Msg(Msg::FollowDependents)
        }
        (Pending::Char('g'), Screen::Graph, Pane::File, KeyInput::Char('g')) => {
            KeyOutcome::Msg(Msg::FileJumpTop)
        }
        (Pending::Char(']'), Screen::Diff, _, KeyInput::Char('c')) => {
            KeyOutcome::Msg(Msg::DiffNextHunk)
        }
        (Pending::Char('['), Screen::Diff, _, KeyInput::Char('c')) => {
            KeyOutcome::Msg(Msg::DiffPrevHunk)
        }
        (Pending::Char(']'), Screen::Diff, _, KeyInput::Char('f')) => {
            KeyOutcome::Msg(Msg::DiffNextFile)
        }
        (Pending::Char('['), Screen::Diff, _, KeyInput::Char('f')) => {
            KeyOutcome::Msg(Msg::DiffPrevFile)
        }
        (Pending::Char(']'), Screen::Graph, Pane::File, KeyInput::Char('c')) => {
            KeyOutcome::Msg(Msg::FileNextChange)
        }
        (Pending::Char('['), Screen::Graph, Pane::File, KeyInput::Char('c')) => {
            KeyOutcome::Msg(Msg::FilePrevChange)
        }
        (Pending::Char(']'), Screen::Graph, Pane::File, KeyInput::Char('f')) => {
            KeyOutcome::Msg(Msg::FileNextFile)
        }
        (Pending::Char('['), Screen::Graph, Pane::File, KeyInput::Char('f')) => {
            KeyOutcome::Msg(Msg::FilePrevFile)
        }
        (Pending::CtrlW, Screen::Graph, _, KeyInput::Char('l'))
        | (Pending::CtrlW, Screen::Graph, _, KeyInput::Arrow(Direction::Right)) => {
            if ctx.file_open {
                KeyOutcome::Msg(Msg::PaneRight)
            } else {
                KeyOutcome::None
            }
        }
        (Pending::CtrlW, Screen::Graph, _, KeyInput::Char('h'))
        | (Pending::CtrlW, Screen::Graph, _, KeyInput::Arrow(Direction::Left)) => {
            KeyOutcome::Msg(Msg::PaneLeft)
        }
        _ => KeyOutcome::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_ctx() -> KeyContext {
        KeyContext {
            screen: Screen::Graph,
            pane: Pane::Graph,
            file_open: false,
            picker_open: false,
            pending: None,
        }
    }

    fn file_pane_ctx() -> KeyContext {
        KeyContext {
            screen: Screen::Graph,
            pane: Pane::File,
            file_open: true,
            picker_open: false,
            pending: None,
        }
    }

    fn diff_ctx() -> KeyContext {
        KeyContext {
            screen: Screen::Diff,
            pane: Pane::Graph,
            file_open: false,
            picker_open: false,
            pending: None,
        }
    }

    fn picker_ctx() -> KeyContext {
        KeyContext {
            screen: Screen::Graph,
            pane: Pane::Graph,
            file_open: false,
            picker_open: true,
            pending: None,
        }
    }

    /// Table-driven: (key, context, expected outcome).
    #[test]
    fn maps_keys_per_context() {
        let cases = [
            // Graph pane: h/j/k/l -> FocusMove, Enter -> OpenFile, d ->
            // OpenDiff.
            (
                KeyInput::Char('h'),
                graph_ctx(),
                KeyOutcome::Msg(Msg::FocusMove(Direction::Left)),
            ),
            (
                KeyInput::Char('j'),
                graph_ctx(),
                KeyOutcome::Msg(Msg::FocusMove(Direction::Down)),
            ),
            (
                KeyInput::Char('k'),
                graph_ctx(),
                KeyOutcome::Msg(Msg::FocusMove(Direction::Up)),
            ),
            (
                KeyInput::Char('l'),
                graph_ctx(),
                KeyOutcome::Msg(Msg::FocusMove(Direction::Right)),
            ),
            (KeyInput::Enter, graph_ctx(), KeyOutcome::Msg(Msg::OpenFile)),
            (
                KeyInput::Char('d'),
                graph_ctx(),
                KeyOutcome::Msg(Msg::OpenDiff),
            ),
            (KeyInput::Esc, graph_ctx(), KeyOutcome::None),
            (KeyInput::Char('q'), graph_ctx(), KeyOutcome::None),
            (
                KeyInput::Char('t'),
                graph_ctx(),
                KeyOutcome::Msg(Msg::ToggleTests),
            ),
            // 'g' and Ctrl-w start chords.
            (
                KeyInput::Char('g'),
                graph_ctx(),
                KeyOutcome::Pending(Pending::Char('g')),
            ),
            (
                KeyInput::Ctrl('w'),
                graph_ctx(),
                KeyOutcome::Pending(Pending::CtrlW),
            ),
            // Picker open: j/k/Enter/Esc, regardless of screen/pane fields.
            (
                KeyInput::Char('j'),
                picker_ctx(),
                KeyOutcome::Msg(Msg::PickerMove(1)),
            ),
            (
                KeyInput::Char('k'),
                picker_ctx(),
                KeyOutcome::Msg(Msg::PickerMove(-1)),
            ),
            (
                KeyInput::Enter,
                picker_ctx(),
                KeyOutcome::Msg(Msg::PickerSelect),
            ),
            (
                KeyInput::Esc,
                picker_ctx(),
                KeyOutcome::Msg(Msg::PickerCancel),
            ),
            (KeyInput::Char('h'), picker_ctx(), KeyOutcome::None),
            // Diff screen: Esc/j/k/s mapped directly, Enter unmapped.
            (KeyInput::Esc, diff_ctx(), KeyOutcome::Msg(Msg::CloseDiff)),
            (
                KeyInput::Char('j'),
                diff_ctx(),
                KeyOutcome::Msg(Msg::DiffScroll(1)),
            ),
            (
                KeyInput::Char('k'),
                diff_ctx(),
                KeyOutcome::Msg(Msg::DiffScroll(-1)),
            ),
            (
                KeyInput::Char('s'),
                diff_ctx(),
                KeyOutcome::Msg(Msg::DiffToggleMode),
            ),
            (
                KeyInput::Char(']'),
                diff_ctx(),
                KeyOutcome::Pending(Pending::Char(']')),
            ),
            (
                KeyInput::Char('['),
                diff_ctx(),
                KeyOutcome::Pending(Pending::Char('[')),
            ),
            (KeyInput::Enter, diff_ctx(), KeyOutcome::None),
            // File pane: j/k -> FileScroll, Ctrl-d/Ctrl-u -> FileHalfPage,
            // G -> FileJumpBottom, d -> OpenDiff, Esc -> CloseFile, Enter
            // unmapped.
            (
                KeyInput::Char('j'),
                file_pane_ctx(),
                KeyOutcome::Msg(Msg::FileScroll(1)),
            ),
            (
                KeyInput::Char('k'),
                file_pane_ctx(),
                KeyOutcome::Msg(Msg::FileScroll(-1)),
            ),
            (
                KeyInput::Ctrl('d'),
                file_pane_ctx(),
                KeyOutcome::Msg(Msg::FileHalfPage(1)),
            ),
            (
                KeyInput::Ctrl('u'),
                file_pane_ctx(),
                KeyOutcome::Msg(Msg::FileHalfPage(-1)),
            ),
            (
                KeyInput::Char('G'),
                file_pane_ctx(),
                KeyOutcome::Msg(Msg::FileJumpBottom),
            ),
            (
                KeyInput::Char('d'),
                file_pane_ctx(),
                KeyOutcome::Msg(Msg::OpenDiff),
            ),
            (
                KeyInput::Esc,
                file_pane_ctx(),
                KeyOutcome::Msg(Msg::CloseFile),
            ),
            (KeyInput::Enter, file_pane_ctx(), KeyOutcome::None),
        ];

        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, ctx), expected, "key={key:?} ctx={ctx:?}");
        }
    }

    #[test]
    fn g_then_d_follows_deps_on_graph_pane() {
        let mut ctx = graph_ctx();
        let outcome = map_key(KeyInput::Char('g'), ctx);
        assert_eq!(outcome, KeyOutcome::Pending(Pending::Char('g')));
        ctx.pending = Some(Pending::Char('g'));
        assert_eq!(
            map_key(KeyInput::Char('d'), ctx),
            KeyOutcome::Msg(Msg::FollowDeps)
        );
    }

    #[test]
    fn g_then_r_follows_dependents_on_graph_pane() {
        let mut ctx = graph_ctx();
        ctx.pending = Some(Pending::Char('g'));
        assert_eq!(
            map_key(KeyInput::Char('r'), ctx),
            KeyOutcome::Msg(Msg::FollowDependents)
        );
    }

    #[test]
    fn g_then_g_jumps_top_on_file_pane() {
        let mut ctx = file_pane_ctx();
        assert_eq!(
            map_key(KeyInput::Char('g'), ctx),
            KeyOutcome::Pending(Pending::Char('g'))
        );
        ctx.pending = Some(Pending::Char('g'));
        assert_eq!(
            map_key(KeyInput::Char('g'), ctx),
            KeyOutcome::Msg(Msg::FileJumpTop)
        );
    }

    #[test]
    fn g_then_d_on_file_pane_is_not_follow_deps() {
        // 'g' means something different per pane -- gd/gr are graph-pane
        // only, so completing with 'd' on the file pane clears the chord
        // rather than firing FollowDeps.
        let mut ctx = file_pane_ctx();
        ctx.pending = Some(Pending::Char('g'));
        assert_eq!(map_key(KeyInput::Char('d'), ctx), KeyOutcome::None);
    }

    #[test]
    fn g_then_anything_else_clears_chord_with_no_message() {
        let mut ctx = graph_ctx();
        ctx.pending = Some(Pending::Char('g'));
        assert_eq!(map_key(KeyInput::Char('x'), ctx), KeyOutcome::None);
        assert_eq!(map_key(KeyInput::Char('g'), ctx), KeyOutcome::None);
        assert_eq!(map_key(KeyInput::Enter, ctx), KeyOutcome::None);
        assert_eq!(map_key(KeyInput::Esc, ctx), KeyOutcome::None);
    }

    #[test]
    fn bracket_c_chords_jump_hunks_on_diff_screen() {
        let mut ctx = diff_ctx();
        assert_eq!(
            map_key(KeyInput::Char(']'), ctx),
            KeyOutcome::Pending(Pending::Char(']'))
        );
        ctx.pending = Some(Pending::Char(']'));
        assert_eq!(
            map_key(KeyInput::Char('c'), ctx),
            KeyOutcome::Msg(Msg::DiffNextHunk)
        );

        ctx.pending = Some(Pending::Char('['));
        assert_eq!(
            map_key(KeyInput::Char('c'), ctx),
            KeyOutcome::Msg(Msg::DiffPrevHunk)
        );
    }

    #[test]
    fn bracket_f_chords_switch_files_on_diff_screen() {
        let mut ctx = diff_ctx();
        ctx.pending = Some(Pending::Char(']'));
        assert_eq!(
            map_key(KeyInput::Char('f'), ctx),
            KeyOutcome::Msg(Msg::DiffNextFile)
        );

        ctx.pending = Some(Pending::Char('['));
        assert_eq!(
            map_key(KeyInput::Char('f'), ctx),
            KeyOutcome::Msg(Msg::DiffPrevFile)
        );
    }

    #[test]
    fn bracket_c_chords_jump_changes_on_file_pane() {
        let mut ctx = file_pane_ctx();
        ctx.pending = Some(Pending::Char(']'));
        assert_eq!(
            map_key(KeyInput::Char('c'), ctx),
            KeyOutcome::Msg(Msg::FileNextChange)
        );
        ctx.pending = Some(Pending::Char('['));
        assert_eq!(
            map_key(KeyInput::Char('c'), ctx),
            KeyOutcome::Msg(Msg::FilePrevChange)
        );
    }

    #[test]
    fn bracket_f_chords_switch_files_on_file_pane() {
        let mut ctx = file_pane_ctx();
        ctx.pending = Some(Pending::Char(']'));
        assert_eq!(
            map_key(KeyInput::Char('f'), ctx),
            KeyOutcome::Msg(Msg::FileNextFile)
        );
        ctx.pending = Some(Pending::Char('['));
        assert_eq!(
            map_key(KeyInput::Char('f'), ctx),
            KeyOutcome::Msg(Msg::FilePrevFile)
        );
    }

    #[test]
    fn bracket_then_anything_else_clears_chord_with_no_message() {
        let mut ctx = diff_ctx();
        ctx.pending = Some(Pending::Char(']'));
        assert_eq!(map_key(KeyInput::Char('x'), ctx), KeyOutcome::None);
        assert_eq!(map_key(KeyInput::Esc, ctx), KeyOutcome::None);
    }

    #[test]
    fn ctrl_w_then_l_switches_pane_right_only_when_file_open() {
        let mut ctx = graph_ctx();
        ctx.pending = Some(Pending::CtrlW);
        assert_eq!(
            map_key(KeyInput::Char('l'), ctx),
            KeyOutcome::None,
            "no file pane open yet"
        );

        ctx.file_open = true;
        assert_eq!(
            map_key(KeyInput::Char('l'), ctx),
            KeyOutcome::Msg(Msg::PaneRight)
        );
    }

    #[test]
    fn ctrl_w_then_h_switches_pane_left_from_either_pane() {
        let mut ctx = file_pane_ctx();
        ctx.pending = Some(Pending::CtrlW);
        assert_eq!(
            map_key(KeyInput::Char('h'), ctx),
            KeyOutcome::Msg(Msg::PaneLeft)
        );
    }

    #[test]
    fn ctrl_w_then_right_arrow_aliases_l() {
        let mut ctx = graph_ctx();
        ctx.pending = Some(Pending::CtrlW);
        assert_eq!(
            map_key(KeyInput::Arrow(Direction::Right), ctx),
            KeyOutcome::None,
            "no file pane open yet"
        );

        ctx.file_open = true;
        assert_eq!(
            map_key(KeyInput::Arrow(Direction::Right), ctx),
            KeyOutcome::Msg(Msg::PaneRight)
        );
    }

    #[test]
    fn ctrl_w_then_left_arrow_aliases_h() {
        let mut ctx = file_pane_ctx();
        ctx.pending = Some(Pending::CtrlW);
        assert_eq!(
            map_key(KeyInput::Arrow(Direction::Left), ctx),
            KeyOutcome::Msg(Msg::PaneLeft)
        );
    }

    #[test]
    fn arrows_are_unbound_outside_a_chord() {
        let ctx = graph_ctx();
        for dir in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            assert_eq!(map_key(KeyInput::Arrow(dir), ctx), KeyOutcome::None);
        }
    }

    #[test]
    fn ctrl_w_then_up_or_down_arrow_clears_chord_with_no_message() {
        let mut ctx = graph_ctx();
        ctx.pending = Some(Pending::CtrlW);
        assert_eq!(
            map_key(KeyInput::Arrow(Direction::Up), ctx),
            KeyOutcome::None
        );
        ctx.pending = Some(Pending::CtrlW);
        assert_eq!(
            map_key(KeyInput::Arrow(Direction::Down), ctx),
            KeyOutcome::None
        );
    }

    #[test]
    fn pending_chord_takes_priority_over_picker_when_picker_closed() {
        // Sanity: pending is only consulted once picker_open is false, which
        // is the only state map_key can be in immediately after a Pending
        // outcome (opening a picker happens via FollowDeps/FollowDependents,
        // which only fire once the chord completes).
        let mut ctx = graph_ctx();
        ctx.pending = Some(Pending::Char('g'));
        assert_eq!(
            map_key(KeyInput::Char('d'), ctx),
            KeyOutcome::Msg(Msg::FollowDeps)
        );
    }
}
