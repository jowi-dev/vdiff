//! Pure `crossterm::event::KeyEvent` -> nvim `nvim_input` angle-bracket
//! notation, the terminal twin of `crate::ui::nvim_pane`'s
//! `translate_event_for_nvim` (egui events -> the same notation). Kept
//! deliberately in agreement with that module's choices -- same special-
//! key spellings, same "shift is already baked into the char, don't
//! double it with `S-`" rule for plain characters -- so an embedded nvim
//! session behaves identically whether it's being driven from the GUI or
//! the TUI.
//!
//! Unlike `crate::tui::keys` (which only needs to recognize the handful of
//! keys `crate::keymap::map_key`'s vdiff chords care about), this module's
//! job is to forward *everything* nvim might want as raw terminal input --
//! every printable character, every modifier combination, every special
//! key -- since nvim itself is the thing interpreting keystrokes once a
//! session is embedded.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Translate one crossterm key event to the `nvim_input`-ready notation
/// string, or `None` if this event carries no input for nvim (a `Release`
/// event -- crossterm's Windows/kitty-protocol backends report these, but
/// nvim's `nvim_input` has no notion of "key released", so there's nothing
/// to send).
pub fn key_event_to_nvim_input(key: &KeyEvent) -> Option<String> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if let Some(special) = special_key_name(key.code) {
        return Some(wrap_special(&special, ctrl, alt));
    }

    match key.code {
        // `Ctrl-Space` has no separate `char` representation crossterm
        // reports consistently across backends -- some report `' '`,
        // nvim's own convention for it is the named `<C-Space>` form, so
        // it's handled ahead of the generic char branch.
        KeyCode::Char(' ') if ctrl => Some(wrap_special("Space", ctrl, alt)),
        KeyCode::Char(c) => {
            if !ctrl && !alt {
                // Shift is already reflected in `c` itself (a real
                // terminal hands crossterm the case-correct/shifted
                // character), so plain chars never get an `S-` prefix --
                // only the literal `<` needs escaping, since nvim would
                // otherwise parse it as the start of a notation sequence.
                return Some(if c == '<' {
                    "<lt>".to_string()
                } else {
                    c.to_string()
                });
            }
            // `<` needs its `lt` spelling even inside a modifier wrap --
            // `<C-<>` would end the notation sequence early.
            let name = if c == '<' {
                "lt".to_string()
            } else {
                c.to_string()
            };
            Some(wrap_special(&name, ctrl, alt))
        }
        _ => None,
    }
}

/// The bare notation name (no angle brackets, no modifier prefix) for a
/// special key, or `None` for anything [`key_event_to_nvim_input`]'s plain-
/// char branch should handle instead.
fn special_key_name(code: KeyCode) -> Option<String> {
    let name = match code {
        KeyCode::Enter => "CR",
        KeyCode::Esc => "Esc",
        KeyCode::Backspace => "BS",
        KeyCode::Tab => "Tab",
        KeyCode::BackTab => "S-Tab",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Delete => "Del",
        KeyCode::Insert => "Insert",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::F(n) => return Some(format!("F{n}")),
        _ => return None,
    };
    Some(name.to_string())
}

/// Wrap `name` in angle brackets, prefixed with `C-`/`M-` (in that order)
/// for whichever of `ctrl`/`alt` is set. `BackTab`'s own `S-Tab` name
/// already carries its `S-`, so a `Ctrl-BackTab` (say) comes out
/// `<C-S-Tab>` -- still C, then M, then S order, S just already baked into
/// `name` here rather than added by this function.
fn wrap_special(name: &str, ctrl: bool, alt: bool) -> String {
    let mut prefix = String::new();
    if ctrl {
        prefix.push_str("C-");
    }
    if alt {
        prefix.push_str("M-");
    }
    format!("<{prefix}{name}>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn with_kind(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn plain_char_passes_through() {
        let key = press(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(key_event_to_nvim_input(&key), Some("a".to_string()));
    }

    #[test]
    fn plain_shifted_char_has_no_extra_s_prefix() {
        // A real terminal hands crossterm the already-shifted char; SHIFT
        // being set alongside it must not add a redundant `<S-A>`.
        let key = press(KeyCode::Char('A'), KeyModifiers::SHIFT);
        assert_eq!(key_event_to_nvim_input(&key), Some("A".to_string()));
    }

    #[test]
    fn less_than_is_escaped() {
        let key = press(KeyCode::Char('<'), KeyModifiers::NONE);
        assert_eq!(key_event_to_nvim_input(&key), Some("<lt>".to_string()));
    }

    #[test]
    fn modified_less_than_uses_lt_spelling() {
        let key = press(KeyCode::Char('<'), KeyModifiers::CONTROL);
        assert_eq!(key_event_to_nvim_input(&key), Some("<C-lt>".to_string()));
    }

    #[test]
    fn ctrl_char_uses_c_notation() {
        let key = press(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(key_event_to_nvim_input(&key), Some("<C-x>".to_string()));
    }

    #[test]
    fn alt_char_uses_m_notation() {
        let key = press(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(key_event_to_nvim_input(&key), Some("<M-x>".to_string()));
    }

    #[test]
    fn ctrl_alt_char_orders_c_then_m() {
        let key = press(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(key_event_to_nvim_input(&key), Some("<C-M-x>".to_string()));
    }

    #[test]
    fn ctrl_space_uses_named_notation() {
        let key = press(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(key_event_to_nvim_input(&key), Some("<C-Space>".to_string()));
    }

    #[test]
    fn plain_space_passes_through_as_char() {
        let key = press(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(key_event_to_nvim_input(&key), Some(" ".to_string()));
    }

    #[test]
    fn special_keys_map_to_expected_notation() {
        let cases = [
            (KeyCode::Enter, "<CR>"),
            (KeyCode::Esc, "<Esc>"),
            (KeyCode::Backspace, "<BS>"),
            (KeyCode::Tab, "<Tab>"),
            (KeyCode::Up, "<Up>"),
            (KeyCode::Down, "<Down>"),
            (KeyCode::Left, "<Left>"),
            (KeyCode::Right, "<Right>"),
            (KeyCode::Delete, "<Del>"),
            (KeyCode::Insert, "<Insert>"),
            (KeyCode::Home, "<Home>"),
            (KeyCode::End, "<End>"),
            (KeyCode::PageUp, "<PageUp>"),
            (KeyCode::PageDown, "<PageDown>"),
            (KeyCode::F(1), "<F1>"),
            (KeyCode::F(12), "<F12>"),
        ];
        for (code, expected) in cases {
            assert_eq!(
                key_event_to_nvim_input(&press(code, KeyModifiers::NONE)),
                Some(expected.to_string()),
                "code={code:?}"
            );
        }
    }

    #[test]
    fn shift_tab_maps_to_s_tab() {
        let key = press(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(key_event_to_nvim_input(&key), Some("<S-Tab>".to_string()));
    }

    #[test]
    fn ctrl_on_a_special_key_prefixes_notation() {
        let key = press(KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(key_event_to_nvim_input(&key), Some("<C-Left>".to_string()));
    }

    #[test]
    fn release_events_produce_no_input() {
        let key = with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(key_event_to_nvim_input(&key), None);
    }

    #[test]
    fn repeat_events_produce_input_like_press() {
        let key = with_kind(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(key_event_to_nvim_input(&key), Some("a".to_string()));
    }

    #[test]
    fn unmapped_key_code_produces_no_input() {
        let key = press(KeyCode::CapsLock, KeyModifiers::NONE);
        assert_eq!(key_event_to_nvim_input(&key), None);
    }
}
