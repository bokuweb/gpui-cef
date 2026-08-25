//! Translates gpui input events into CEF events.
//!
//! CEF speaks Windows virtual key codes (VK_*), so key names that gpui reports
//! on macOS are mapped over to those here.

use cef::{KeyEvent, KeyEventType, MouseButtonType, MouseEvent};
use gpui::{Bounds, Modifiers, MouseButton, Pixels, Point};

// CEF's cef_event_flags_t. The sys side is an enum, so redefine them as plain
// u32 values to keep the bit twiddling readable.
const EVENTFLAG_SHIFT_DOWN: u32 = 1 << 1;
const EVENTFLAG_CONTROL_DOWN: u32 = 1 << 2;
const EVENTFLAG_ALT_DOWN: u32 = 1 << 3;
const EVENTFLAG_LEFT_MOUSE_BUTTON: u32 = 1 << 4;
const EVENTFLAG_MIDDLE_MOUSE_BUTTON: u32 = 1 << 5;
const EVENTFLAG_RIGHT_MOUSE_BUTTON: u32 = 1 << 6;
const EVENTFLAG_COMMAND_DOWN: u32 = 1 << 7;

/// gpui modifier state to CEF's `modifiers` bits.
pub(crate) fn modifiers(modifiers: &Modifiers) -> u32 {
    let mut flags = 0;
    if modifiers.shift {
        flags |= EVENTFLAG_SHIFT_DOWN;
    }
    if modifiers.control {
        flags |= EVENTFLAG_CONTROL_DOWN;
    }
    if modifiers.alt {
        flags |= EVENTFLAG_ALT_DOWN;
    }
    if modifiers.platform {
        // Command on macOS, which CEF calls COMMAND_DOWN.
        flags |= EVENTFLAG_COMMAND_DOWN;
    }
    flags
}

/// Adds the currently held mouse button to CEF's `modifiers` bits.
pub(crate) fn with_pressed_button(flags: u32, button: Option<MouseButton>) -> u32 {
    match button {
        Some(MouseButton::Left) => flags | EVENTFLAG_LEFT_MOUSE_BUTTON,
        Some(MouseButton::Middle) => flags | EVENTFLAG_MIDDLE_MOUSE_BUTTON,
        Some(MouseButton::Right) => flags | EVENTFLAG_RIGHT_MOUSE_BUTTON,
        _ => flags,
    }
}

/// Converts a window-space mouse position into the webview's view space (DIP).
pub(crate) fn mouse_event(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    modifier_flags: u32,
) -> MouseEvent {
    MouseEvent {
        x: f32::from(position.x - bounds.origin.x).round() as i32,
        y: f32::from(position.y - bounds.origin.y).round() as i32,
        modifiers: modifier_flags,
    }
}

/// gpui's `MouseButton` to CEF's button kind. Navigation buttons are ignored.
pub(crate) fn mouse_button(button: MouseButton) -> Option<MouseButtonType> {
    match button {
        MouseButton::Left => Some(MouseButtonType::LEFT),
        MouseButton::Middle => Some(MouseButtonType::MIDDLE),
        MouseButton::Right => Some(MouseButtonType::RIGHT),
        MouseButton::Navigate(_) => None,
    }
}

/// Builds a CEF `KeyEvent`. Leaving `size` unset gets the event rejected.
fn new_key_event(type_: KeyEventType, modifier_flags: u32) -> KeyEvent {
    KeyEvent {
        size: std::mem::size_of::<cef::sys::_cef_key_event_t>(),
        type_,
        modifiers: modifier_flags,
        ..Default::default()
    }
}

/// Builds the events to send to CEF for a key press.
///
/// CEF expects two stages: RAWKEYDOWN for the physical key, then CHAR for the
/// text it produced. Keys that type nothing (arrows, Escape) get no CHAR.
pub(crate) fn key_down_events(keystroke: &gpui::Keystroke) -> Vec<KeyEvent> {
    let flags = modifiers(&keystroke.modifiers);
    let vk = windows_key_code(&keystroke.key);
    let mut events = Vec::with_capacity(2);

    let mut raw = new_key_event(KeyEventType::RAWKEYDOWN, flags);
    raw.windows_key_code = vk;
    raw.native_key_code = vk;
    events.push(raw);

    // Modifier-only presses and command shortcuts do not produce text.
    if !keystroke.modifiers.control && !keystroke.modifiers.platform {
        if let Some(text) = keystroke.key_char.as_deref() {
            for unit in text.encode_utf16() {
                let mut char_event = new_key_event(KeyEventType::CHAR, flags);
                char_event.windows_key_code = unit as i32;
                char_event.character = unit;
                char_event.unmodified_character = unit;
                events.push(char_event);
            }
        }
    }

    events
}

/// The matching key release event.
pub(crate) fn key_up_event(keystroke: &gpui::Keystroke) -> KeyEvent {
    let vk = windows_key_code(&keystroke.key);
    let mut event = new_key_event(KeyEventType::KEYUP, modifiers(&keystroke.modifiers));
    event.windows_key_code = vk;
    event.native_key_code = vk;
    event
}

/// gpui key name to Windows virtual key code.
///
/// gpui normalizes key names even on macOS ("enter", "escape", ...), so they are
/// mapped here onto the VK_* values CEF (that is, Chromium) expects.
fn windows_key_code(key: &str) -> i32 {
    match key {
        "backspace" => 0x08,
        "tab" => 0x09,
        "enter" => 0x0D,
        "shift" => 0x10,
        "ctrl" | "control" => 0x11,
        "alt" => 0x12,
        "capslock" => 0x14,
        "escape" => 0x1B,
        "space" => 0x20,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "cmd" | "platform" => 0x5B,
        "-" => 0xBD,
        "=" => 0xBB,
        "[" => 0xDB,
        "]" => 0xDD,
        "\\" => 0xDC,
        ";" => 0xBA,
        "'" => 0xDE,
        "," => 0xBC,
        "." => 0xBE,
        "/" => 0xBF,
        "`" => 0xC0,
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                // "a".."z" and "0".."9" map to their uppercase ASCII value.
                (Some(c), None) if c.is_ascii_alphanumeric() => c.to_ascii_uppercase() as i32,
                // "f1".."f24" run consecutively from VK_F1 (0x70).
                (Some('f'), Some(_)) => match key[1..].parse::<i32>() {
                    Ok(n) if (1..=24).contains(&n) => 0x70 + n - 1,
                    _ => 0,
                },
                _ => 0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size, Keystroke};

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    fn bounds(x: f32, y: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(800.), px(600.)),
        }
    }

    #[test]
    fn mouse_position_is_relative_to_the_element() {
        // The webview rarely sits at the window origin, so the offset has to go.
        let event = mouse_event(point(px(140.), px(90.)), bounds(100., 50.), 0);
        assert_eq!((event.x, event.y), (40, 40));
    }

    #[test]
    fn mouse_position_rounds_rather_than_truncates() {
        let event = mouse_event(point(px(10.6), px(10.4)), bounds(0., 0.), 0);
        assert_eq!((event.x, event.y), (11, 10));
    }

    #[test]
    fn modifiers_map_to_cef_flags() {
        let mods = Modifiers {
            shift: true,
            platform: true,
            ..Default::default()
        };
        let flags = modifiers(&mods);
        assert_eq!(flags & EVENTFLAG_SHIFT_DOWN, EVENTFLAG_SHIFT_DOWN);
        assert_eq!(flags & EVENTFLAG_COMMAND_DOWN, EVENTFLAG_COMMAND_DOWN);
        assert_eq!(flags & EVENTFLAG_CONTROL_DOWN, 0);
    }

    #[test]
    fn held_button_is_added_to_the_flags() {
        assert_eq!(
            with_pressed_button(0, Some(MouseButton::Left)),
            EVENTFLAG_LEFT_MOUSE_BUTTON
        );
        // Drag events carry no button while the mouse is merely moving.
        assert_eq!(with_pressed_button(0, None), 0);
        // Navigation buttons have no CEF flag, so they must not corrupt the mask.
        assert_eq!(
            with_pressed_button(
                0,
                Some(MouseButton::Navigate(gpui::NavigationDirection::Back))
            ),
            0
        );
    }

    #[test]
    fn typing_sends_a_raw_key_and_a_char() {
        let events = key_down_events(&keystroke("a", Some("a"), Modifiers::default()));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].type_, KeyEventType::RAWKEYDOWN);
        assert_eq!(events[0].windows_key_code, 'A' as i32);
        assert_eq!(events[1].type_, KeyEventType::CHAR);
        assert_eq!(events[1].character, 'a' as u16);
    }

    #[test]
    fn shortcuts_do_not_type_text() {
        // Cmd-A selects all; it must not also insert an "a" into the page.
        let events = key_down_events(&keystroke(
            "a",
            Some("a"),
            Modifiers {
                platform: true,
                ..Default::default()
            },
        ));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].type_, KeyEventType::RAWKEYDOWN);
    }

    #[test]
    fn keys_without_text_send_no_char() {
        let events = key_down_events(&keystroke("left", None, Modifiers::default()));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].windows_key_code, 0x25);
    }

    #[test]
    fn astral_characters_become_a_surrogate_pair() {
        // CEF's KeyEvent::character is a single UTF-16 unit, so an emoji needs two.
        let events = key_down_events(&keystroke("u", Some("\u{1F600}"), Modifiers::default()));
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].character, 0xD83D);
        assert_eq!(events[2].character, 0xDE00);
    }

    #[test]
    fn key_events_carry_their_size() {
        // CEF rejects the event outright when size is left at zero.
        let events = key_down_events(&keystroke("a", Some("a"), Modifiers::default()));
        let expected = std::mem::size_of::<cef::sys::_cef_key_event_t>();
        assert!(events.iter().all(|event| event.size == expected));
        assert_eq!(
            key_up_event(&keystroke("a", Some("a"), Modifiers::default())).size,
            expected
        );
    }

    #[test]
    fn named_keys_map_to_virtual_key_codes() {
        assert_eq!(windows_key_code("enter"), 0x0D);
        assert_eq!(windows_key_code("escape"), 0x1B);
        assert_eq!(windows_key_code("backspace"), 0x08);
        assert_eq!(windows_key_code("f1"), 0x70);
        assert_eq!(windows_key_code("f12"), 0x7B);
        assert_eq!(windows_key_code("\\"), 0xDC);
    }

    #[test]
    fn letters_and_digits_use_their_ascii_uppercase() {
        assert_eq!(windows_key_code("z"), 'Z' as i32);
        assert_eq!(windows_key_code("7"), '7' as i32);
    }

    #[test]
    fn unknown_keys_map_to_zero_instead_of_panicking() {
        // gpui reports layout-specific names this table does not cover.
        assert_eq!(windows_key_code("f99"), 0);
        assert_eq!(windows_key_code(""), 0);
        assert_eq!(windows_key_code("unknownkey"), 0);
        // Multi-byte input must not panic on a byte-wise slice.
        assert_eq!(windows_key_code("あ"), 0);
    }
}
