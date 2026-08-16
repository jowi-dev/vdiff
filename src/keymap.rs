//! Pure `KeyInput -> Msg` mapping, independent of any GUI toolkit's key
//! event type. `map_key` never touches `App` state directly; the caller
//! threads `KeyContext` in (current screen, whether a picker is open) and
//! carries `pending` across calls to implement the two-keystroke `gd`/`gr`
//! chords.

use crate::core::app::{Msg, Screen};
use crate::core::focus::Direction;

/// A single keypress, abstracted away from any GUI toolkit's key event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInput {
    /// A printable character key.
    Char(char),
    /// The Enter/Return key.
    Enter,
    /// The Escape key.
    Esc,
}

/// Everything `map_key` needs besides the keypress itself: where in the app
/// the key landed, and any pending prefix key from the previous call (see
/// [`KeyOutcome::Pending`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyContext {
    /// The screen currently shown.
    pub screen: Screen,
    /// Whether the edge-following picker overlay is open. Checked ahead of
    /// `screen` -- the picker only ever opens over [`Screen::Graph`], but
    /// its keys take priority regardless.
    pub picker_open: bool,
    /// A prefix key returned as [`KeyOutcome::Pending`] by the previous
    /// call, or `None` if no chord is in progress.
    pub pending: Option<char>,
}

/// The result of [`map_key`]: either a [`Msg`] to dispatch, a prefix key to
/// remember and pass back in as `KeyContext::pending` on the next call, or
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Dispatch this message.
    Msg(Msg),
    /// `key` started a chord (currently only `g`); remember it and pass it
    /// back in as `pending` on the next keypress.
    Pending(char),
    /// No mapping for this key in this context.
    None,
}

/// Map a keypress to a [`KeyOutcome`], per `ctx`.
///
/// Precedence:
/// 1. `ctx.picker_open` -- `j`/`k` move the selection, `Enter` selects,
///    `Esc` cancels; everything else is unmapped.
/// 2. `ctx.pending == Some('g')` -- completes the `gd`/`gr` chord
///    ([`Msg::FollowDeps`]/[`Msg::FollowDependents`]); any other key clears
///    the chord with no message.
/// 3. Otherwise, per `ctx.screen`:
///    - [`Screen::Graph`]: `h`/`j`/`k`/`l` -> [`Msg::FocusMove`], `Enter` ->
///      [`Msg::OpenDiff`], `g` -> [`KeyOutcome::Pending`].
///    - [`Screen::Diff`]: `Esc` -> [`Msg::CloseDiff`]. Scrolling and
///      hunk-jumping keys arrive in a later chunk.
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

    if let Some(prefix) = ctx.pending {
        return match (prefix, key) {
            ('g', KeyInput::Char('d')) => KeyOutcome::Msg(Msg::FollowDeps),
            ('g', KeyInput::Char('r')) => KeyOutcome::Msg(Msg::FollowDependents),
            _ => KeyOutcome::None,
        };
    }

    match ctx.screen {
        Screen::Graph => match key {
            KeyInput::Char('h') => KeyOutcome::Msg(Msg::FocusMove(Direction::Left)),
            KeyInput::Char('j') => KeyOutcome::Msg(Msg::FocusMove(Direction::Down)),
            KeyInput::Char('k') => KeyOutcome::Msg(Msg::FocusMove(Direction::Up)),
            KeyInput::Char('l') => KeyOutcome::Msg(Msg::FocusMove(Direction::Right)),
            KeyInput::Enter => KeyOutcome::Msg(Msg::OpenDiff),
            KeyInput::Char('g') => KeyOutcome::Pending('g'),
            _ => KeyOutcome::None,
        },
        Screen::Diff => match key {
            KeyInput::Esc => KeyOutcome::Msg(Msg::CloseDiff),
            _ => KeyOutcome::None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_ctx() -> KeyContext {
        KeyContext {
            screen: Screen::Graph,
            picker_open: false,
            pending: None,
        }
    }

    fn diff_ctx() -> KeyContext {
        KeyContext {
            screen: Screen::Diff,
            picker_open: false,
            pending: None,
        }
    }

    fn picker_ctx() -> KeyContext {
        KeyContext {
            screen: Screen::Graph,
            picker_open: true,
            pending: None,
        }
    }

    /// Table-driven: (key, context, expected outcome).
    #[test]
    fn maps_keys_per_context() {
        let cases = [
            // Graph, no picker: h/j/k/l -> FocusMove, Enter -> OpenDiff.
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
            (KeyInput::Enter, graph_ctx(), KeyOutcome::Msg(Msg::OpenDiff)),
            (KeyInput::Esc, graph_ctx(), KeyOutcome::None),
            (KeyInput::Char('q'), graph_ctx(), KeyOutcome::None),
            // 'g' starts a chord.
            (KeyInput::Char('g'), graph_ctx(), KeyOutcome::Pending('g')),
            // Picker open: j/k/Enter/Esc, regardless of screen field.
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
            // Diff screen: only Esc is mapped.
            (KeyInput::Esc, diff_ctx(), KeyOutcome::Msg(Msg::CloseDiff)),
            (KeyInput::Char('j'), diff_ctx(), KeyOutcome::None),
            (KeyInput::Enter, diff_ctx(), KeyOutcome::None),
        ];

        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, ctx), expected, "key={key:?} ctx={ctx:?}");
        }
    }

    #[test]
    fn g_then_d_follows_deps() {
        let mut ctx = graph_ctx();
        let outcome = map_key(KeyInput::Char('g'), ctx);
        assert_eq!(outcome, KeyOutcome::Pending('g'));
        ctx.pending = Some('g');
        assert_eq!(
            map_key(KeyInput::Char('d'), ctx),
            KeyOutcome::Msg(Msg::FollowDeps)
        );
    }

    #[test]
    fn g_then_r_follows_dependents() {
        let mut ctx = graph_ctx();
        ctx.pending = Some('g');
        assert_eq!(
            map_key(KeyInput::Char('r'), ctx),
            KeyOutcome::Msg(Msg::FollowDependents)
        );
    }

    #[test]
    fn g_then_anything_else_clears_chord_with_no_message() {
        let mut ctx = graph_ctx();
        ctx.pending = Some('g');
        assert_eq!(map_key(KeyInput::Char('x'), ctx), KeyOutcome::None);
        assert_eq!(map_key(KeyInput::Char('g'), ctx), KeyOutcome::None);
        assert_eq!(map_key(KeyInput::Enter, ctx), KeyOutcome::None);
        assert_eq!(map_key(KeyInput::Esc, ctx), KeyOutcome::None);
    }

    #[test]
    fn pending_chord_takes_priority_over_picker_when_picker_closed() {
        // Sanity: pending is only consulted once picker_open is false, which
        // is the only state map_key can be in immediately after a Pending
        // outcome (opening a picker happens via FollowDeps/FollowDependents,
        // which only fire once the chord completes).
        let mut ctx = graph_ctx();
        ctx.pending = Some('g');
        assert_eq!(
            map_key(KeyInput::Char('d'), ctx),
            KeyOutcome::Msg(Msg::FollowDeps)
        );
    }
}
