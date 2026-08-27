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

/// The physical key press.
///
pub(crate) fn key_down_event(keystroke: &gpui::Keystroke) -> KeyEvent {
    let vk = windows_key_code(&keystroke.key);
    let mut event = new_key_event(KeyEventType::RAWKEYDOWN, modifiers(&keystroke.modifiers));
    event.windows_key_code = vk;
    event.native_key_code = native_key_code(&keystroke.key, vk);
    event.character = first_utf16(keystroke.key_char.as_deref().unwrap_or_default());
    event.unmodified_character = first_utf16(&keystroke.key);
    event
}

/// Whether a key should wait for AppKit to resolve it as committed text.
///
/// Return and Tab have a `key_char` in gpui, but AppKit handles them as editing
/// commands rather than calling `insertText:`. Holding those events would
/// therefore prevent CEF from ever seeing their physical key press.
pub(crate) fn should_wait_for_text_input(keystroke: &gpui::Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    !modifiers.control
        && !modifiers.platform
        && !modifiers.function
        && keystroke.key_char.as_deref().is_some_and(|text| {
            !text.is_empty() && text.chars().all(|character| !character.is_control())
        })
}

/// Character input committed by the platform text input client.
///
/// CEF does not derive text from `RAWKEYDOWN` for a windowless browser. GPUI's
/// input handler calls this only after AppKit has decided the keystroke is text,
/// so shortcuts never become accidental character input.
pub(crate) fn text_events(text: &str) -> Vec<KeyEvent> {
    text.encode_utf16()
        .map(|character| {
            // AppKit reports Return as LF, while Chromium's keyboard path uses
            // CR for the corresponding CHAR event.
            let character = if character == b'\n' as u16 {
                b'\r' as u16
            } else {
                character
            };
            let mut event = new_key_event(KeyEventType::CHAR, 0);
            event.windows_key_code = character as i32;
            event.character = character;
            event.unmodified_character = character;
            event
        })
        .collect()
}

/// The matching key release event.
pub(crate) fn key_up_event(keystroke: &gpui::Keystroke) -> KeyEvent {
    let vk = windows_key_code(&keystroke.key);
    let mut event = new_key_event(KeyEventType::KEYUP, modifiers(&keystroke.modifiers));
    event.windows_key_code = vk;
    event.native_key_code = native_key_code(&keystroke.key, vk);
    event.character = first_utf16(keystroke.key_char.as_deref().unwrap_or_default());
    event.unmodified_character = first_utf16(&keystroke.key);
    event
}

fn first_utf16(text: &str) -> u16 {
    text.encode_utf16().next().unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn native_key_code(key: &str, _windows_key_code: i32) -> i32 {
    match key {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "enter" => 36,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "tab" => 48,
        "space" => 49,
        "`" => 50,
        "backspace" => 51,
        "escape" => 53,
        "cmd" | "platform" => 55,
        "shift" => 56,
        "capslock" => 57,
        "alt" => 58,
        "ctrl" | "control" => 59,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f3" => 99,
        "f8" => 100,
        "f9" => 101,
        "f11" => 103,
        "f13" => 105,
        "f16" => 106,
        "f14" => 107,
        "f10" => 109,
        "f12" => 111,
        "f15" => 113,
        "insert" => 114,
        "home" => 115,
        "pageup" => 116,
        "delete" => 117,
        "f4" => 118,
        "end" => 119,
        "f2" => 120,
        "pagedown" => 121,
        "f1" => 122,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => 0,
    }
}

#[cfg(not(target_os = "macos"))]
fn native_key_code(_key: &str, windows_key_code: i32) -> i32 {
    windows_key_code
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
    fn a_key_press_maps_the_physical_key() {
        let event = key_down_event(&keystroke("a", Some("a"), Modifiers::default()));
        assert_eq!(event.type_, KeyEventType::RAWKEYDOWN);
        assert_eq!(event.windows_key_code, 'A' as i32);
        assert_eq!(event.character, 'a' as u16);
        assert_eq!(event.unmodified_character, 'a' as u16);
    }

    #[test]
    fn printable_keys_wait_for_appkit_text_input() {
        assert!(should_wait_for_text_input(&keystroke(
            "a",
            Some("a"),
            Modifiers::default()
        )));
        assert!(should_wait_for_text_input(&keystroke(
            "space",
            Some(" "),
            Modifiers::default()
        )));
    }

    #[test]
    fn editing_commands_do_not_wait_for_text_input() {
        assert!(!should_wait_for_text_input(&keystroke(
            "enter",
            Some("\n"),
            Modifiers::default()
        )));
        assert!(!should_wait_for_text_input(&keystroke(
            "tab",
            Some("\t"),
            Modifiers::default()
        )));
    }

    #[test]
    fn committed_text_emits_character_input() {
        let events = text_events("a");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].type_, KeyEventType::CHAR);
        assert_eq!(events[0].character, 'a' as u16);
    }

    #[test]
    fn committed_unicode_preserves_utf16() {
        let events = text_events("日本");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].character, '日' as u16);
        assert_eq!(events[1].character, '本' as u16);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_native_hardware_key_codes() {
        let event = key_down_event(&keystroke("x", Some("x"), Modifiers::default()));
        assert_eq!(event.windows_key_code, 'X' as i32);
        assert_eq!(event.native_key_code, 7);
    }

    #[test]
    fn shortcuts_keep_their_modifiers() {
        // Cmd-A has to reach the page as a shortcut, not as text.
        let event = key_down_event(&keystroke(
            "a",
            Some("a"),
            Modifiers {
                platform: true,
                ..Default::default()
            },
        ));
        assert_eq!(
            event.modifiers & EVENTFLAG_COMMAND_DOWN,
            EVENTFLAG_COMMAND_DOWN
        );
    }

    #[test]
    fn keys_without_text_still_reach_the_page() {
        let event = key_down_event(&keystroke("left", None, Modifiers::default()));
        assert_eq!(event.windows_key_code, 0x25);
    }

    #[test]
    fn key_events_carry_their_size() {
        // CEF rejects the event outright when size is left at zero.
        let expected = std::mem::size_of::<cef::sys::_cef_key_event_t>();
        assert_eq!(
            key_down_event(&keystroke("a", Some("a"), Modifiers::default())).size,
            expected
        );
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
