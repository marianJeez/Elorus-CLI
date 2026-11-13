//! Keys, pastes, resizes and the mouse, as a stream of what the UI reacts to.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use crossterm::event::{Event, EventStream, MouseButton, MouseEventKind};
// CEILING: key types are crossterm's; a backend swap needs a mirror type.
pub use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::{Stream, StreamExt};

use crate::data;

/// Every layout's letters that stand for a Latin key, from data/key_layouts.toml.
static LAYOUTS: LazyLock<BTreeMap<String, BTreeMap<char, char>>> = data::load!("key_layouts.toml");

/// What the UI reacts to.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    Key(KeyEvent),
    Paste(String),
    Resize(usize, usize),
    /// Wheel notches: negative is up.
    Scroll(isize),
    /// A press on a screen row, zero-based.
    Click {
        row: usize,
        button: Button,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

/// Terminal input as a stream.
pub fn inputs() -> impl Stream<Item = Input> {
    EventStream::new().filter_map(|event| async move { input(event.ok()?) })
}

/// Key releases are dropped (Windows reports one for every key, which would otherwise act
/// twice), and so are mouse moves, drags and the middle button.
fn input(event: Event) -> Option<Input> {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => Some(Input::Key(qwerty(key))),
        Event::Paste(text) => Some(Input::Paste(text)),
        Event::Resize(w, h) => Some(Input::Resize(w as usize, h as usize)),
        Event::Mouse(mouse) => {
            let button = match mouse.kind {
                MouseEventKind::ScrollUp => return Some(Input::Scroll(-1)),
                MouseEventKind::ScrollDown => return Some(Input::Scroll(1)),
                MouseEventKind::Down(MouseButton::Left) => Button::Left,
                MouseEventKind::Down(MouseButton::Right) => Button::Right,
                _ => return None,
            };
            Some(Input::Click {
                row: mouse.row as usize,
                button,
            })
        }
        _ => None,
    }
}

/// Ctrl+letter as the QWERTY key carrying it: Windows reports the active layout's letter
/// (`в` for Ctrl+D in Cyrillic), while every binding in data/keybinds.toml is Latin. Plain
/// typing is left alone, so the layout's own letters still reach the editor.
fn qwerty(mut key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return key;
    }
    let KeyCode::Char(c) = key.code else {
        return key;
    };
    let lower = c.to_lowercase().next().unwrap_or(c);
    if let Some(latin) = LAYOUTS.values().find_map(|letters| letters.get(&lower)) {
        key.code = KeyCode::Char(*latin);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::MouseEvent;

    fn mouse(kind: MouseEventKind, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn a_ctrl_letter_from_another_layout_is_the_qwerty_key_and_plain_typing_is_not() {
        let press = |c, modifiers| {
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char(c),
                modifiers,
                KeyEventKind::Press,
            ))
        };
        let key = |code, modifiers| {
            Some(Input::Key(KeyEvent::new_with_kind(
                code,
                modifiers,
                KeyEventKind::Press,
            )))
        };
        // Ctrl+D on a Cyrillic layout, with and without Shift.
        assert_eq!(
            input(press('в', KeyModifiers::CONTROL)),
            key(KeyCode::Char('d'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            input(press('В', KeyModifiers::CONTROL | KeyModifiers::SHIFT)),
            key(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
        );
        // The same letter without Ctrl is text.
        assert_eq!(
            input(press('в', KeyModifiers::NONE)),
            key(KeyCode::Char('в'), KeyModifiers::NONE)
        );
    }

    #[test]
    fn both_buttons_and_the_wheel_arrive_and_the_rest_of_the_mouse_does_not() {
        let click = |button| Some(Input::Click { row: 4, button });
        assert_eq!(
            input(mouse(MouseEventKind::Down(MouseButton::Left), 4)),
            click(Button::Left)
        );
        assert_eq!(
            input(mouse(MouseEventKind::Down(MouseButton::Right), 4)),
            click(Button::Right)
        );
        assert_eq!(
            input(mouse(MouseEventKind::ScrollUp, 0)),
            Some(Input::Scroll(-1))
        );
        assert_eq!(
            input(mouse(MouseEventKind::Down(MouseButton::Middle), 4)),
            None
        );
        assert_eq!(
            input(mouse(MouseEventKind::Drag(MouseButton::Left), 4)),
            None
        );
    }
}
