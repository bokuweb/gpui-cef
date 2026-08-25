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
