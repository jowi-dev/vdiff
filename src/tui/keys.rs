//! Pure `crossterm::event::KeyEvent -> KeyInput` mapping, the terminal
//! twin of `crate::ui::eframe_app::egui_key_to_input`. Both feed the same
//! toolkit-neutral [`crate::keymap::KeyInput`]/[`crate::keymap::map_key`]
//! pipeline, so vdiff's chord state machine (`gd`/`gr`/`gg`, `]c`/`[c`,
//! `]f`/`[f`, `Ctrl-w h`/`Ctrl-w l`) ports unchanged: this module's only job
//! is deciding which raw terminal keypress corresponds to which
//! [`KeyInput`] variant.
//!
//! Unlike egui (whose `Key::G` carries no case of its own -- the caller has
//! to consult a separate shift modifier), crossterm's `KeyCode::Char`
//! already reports the terminal's own case-correct character (a real
//! terminal decodes Shift-g to `'G'` before crossterm ever sees it), so
//! there's no shift-modifier branching needed here for letters at all.

use crossterm::event::{KeyCode, KeyModifiers};

use crate::core::focus::Direction;
use crate::keymap::KeyInput;

/// Translate one crossterm keypress to vdiff's toolkit-independent
/// [`KeyInput`], or `None` if it maps to nothing [`crate::keymap::map_key`]
/// cares about. Arrow keys map to [`KeyInput::Arrow`] regardless of
/// modifiers (checked first, so a `Ctrl` held alongside an arrow --
/// completing the `Ctrl-w` pane-switch chord via an arrow key, same as the
/// GUI -- still resolves); with `Ctrl` held otherwise, only `w`/`d`/`u` map
/// to anything ([`KeyInput::Ctrl`]); everything else falls through to the
/// plain-character table [`map_key`] actually understands
/// (h/j/k/l/g/G/d/r/t/s/c/v/f/[/]), plus `Enter`/`Esc`.
pub fn crossterm_key_to_input(code: KeyCode, modifiers: KeyModifiers) -> Option<KeyInput> {
    if let Some(dir) = arrow_direction(code) {
        return Some(KeyInput::Arrow(dir));
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        return match code {
            KeyCode::Char('w') => Some(KeyInput::Ctrl('w')),
            KeyCode::Char('d') => Some(KeyInput::Ctrl('d')),
            KeyCode::Char('u') => Some(KeyInput::Ctrl('u')),
            // `Ctrl-e` isn't part of `crate::keymap` at all -- it's the
            // TUI-only "edit in real nvim" handoff (see
            // `crate::tui::nvim_handoff`), intercepted by the event loop
            // before this even reaches `map_key`. Mapping it here anyway
            // (rather than returning `None`) lets that interception check
            // happen in the same `KeyInput`-typed comparison as every other
            // binding, instead of a separate raw-crossterm special case.
            KeyCode::Char('e') => Some(KeyInput::Ctrl('e')),
            _ => None,
        };
    }
    match code {
        KeyCode::Char(
            c @ ('h' | 'j' | 'k' | 'l' | 'g' | 'G' | 'd' | 'r' | 't' | 's' | 'c' | 'v' | 'f' | '['
            | ']'
            // `` ` `` and `z` aren't part of `crate::keymap::map_key`'s
            // shared vocabulary at all -- both are intercepted directly by
            // `crate::tui::handle_key` before `map_key` ever sees them
            // (the view-mode toggle and the canvas view's `zc`/`zo` fold
            // chord, respectively -- see that module's doc). Mapped here
            // anyway, rather than left as `None`, for the same reason
            // `Ctrl-e` is: so the interception check happens in the same
            // `KeyInput`-typed comparison as every other binding.
            | '`' | 'o' | 'z'),
        ) => Some(KeyInput::Char(c)),
        KeyCode::Enter => Some(KeyInput::Enter),
        KeyCode::Esc => Some(KeyInput::Esc),
        _ => None,
    }
}

/// Arrow keys to [`Direction`], or `None` for any other key code.
fn arrow_direction(code: KeyCode) -> Option<Direction> {
    match code {
        KeyCode::Left => Some(Direction::Left),
        KeyCode::Right => Some(Direction::Right),
        KeyCode::Up => Some(Direction::Up),
        KeyCode::Down => Some(Direction::Down),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_plain_letters_with_no_modifiers() {
        let cases = [
            ('h', KeyInput::Char('h')),
            ('j', KeyInput::Char('j')),
            ('k', KeyInput::Char('k')),
            ('l', KeyInput::Char('l')),
            ('g', KeyInput::Char('g')),
            ('G', KeyInput::Char('G')),
            ('d', KeyInput::Char('d')),
            ('r', KeyInput::Char('r')),
            ('t', KeyInput::Char('t')),
            ('s', KeyInput::Char('s')),
            ('c', KeyInput::Char('c')),
            ('v', KeyInput::Char('v')),
            ('f', KeyInput::Char('f')),
            ('[', KeyInput::Char('[')),
            (']', KeyInput::Char(']')),
            ('`', KeyInput::Char('`')),
            ('z', KeyInput::Char('z')),
            ('o', KeyInput::Char('o')),
        ];
        for (c, expected) in cases {
            assert_eq!(
                crossterm_key_to_input(KeyCode::Char(c), KeyModifiers::NONE),
                Some(expected),
                "char={c:?}"
            );
        }
    }

    #[test]
    fn maps_enter_and_esc() {
        assert_eq!(
            crossterm_key_to_input(KeyCode::Enter, KeyModifiers::NONE),
            Some(KeyInput::Enter)
        );
        assert_eq!(
            crossterm_key_to_input(KeyCode::Esc, KeyModifiers::NONE),
            Some(KeyInput::Esc)
        );
    }

    #[test]
    fn ctrl_modifier_maps_w_d_u_e_and_nothing_else() {
        assert_eq!(
            crossterm_key_to_input(KeyCode::Char('w'), KeyModifiers::CONTROL),
            Some(KeyInput::Ctrl('w'))
        );
        assert_eq!(
            crossterm_key_to_input(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Some(KeyInput::Ctrl('d'))
        );
        assert_eq!(
            crossterm_key_to_input(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Some(KeyInput::Ctrl('u'))
        );
        assert_eq!(
            crossterm_key_to_input(KeyCode::Char('e'), KeyModifiers::CONTROL),
            Some(KeyInput::Ctrl('e'))
        );
        assert_eq!(
            crossterm_key_to_input(KeyCode::Char('h'), KeyModifiers::CONTROL),
            None
        );
    }

    #[test]
    fn unmapped_keys_translate_to_none() {
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char('q'),
            KeyCode::Char('1'),
            KeyCode::Tab,
            KeyCode::Backspace,
        ] {
            assert_eq!(
                crossterm_key_to_input(code, KeyModifiers::NONE),
                None,
                "code={code:?}"
            );
        }
    }

    #[test]
    fn arrow_keys_translate_regardless_of_ctrl() {
        let cases = [
            (KeyCode::Left, Direction::Left),
            (KeyCode::Right, Direction::Right),
            (KeyCode::Up, Direction::Up),
            (KeyCode::Down, Direction::Down),
        ];
        for (code, dir) in cases {
            assert_eq!(
                crossterm_key_to_input(code, KeyModifiers::NONE),
                Some(KeyInput::Arrow(dir))
            );
            assert_eq!(
                crossterm_key_to_input(code, KeyModifiers::CONTROL),
                Some(KeyInput::Arrow(dir))
            );
        }
    }
}
