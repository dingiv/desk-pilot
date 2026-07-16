//! Pure-logic mapping from VRover's logical [`Key`]/[`Button`] to Linux
//! `input-event-codes.h` constants (`KEY_*` / `BTN_*`). Always compiled + unit
//! tested — it is the testable core of this crate; the evdev device glue that
//! *sends* these codes lives in [`crate::backend`] behind the `backend` feature.
//!
//! Values are the raw u16 event codes from `linux/input-event-codes.h`. Duplicating
//! them here (rather than depending on a codes crate) keeps the mapping testable
//! without pulling native deps.

use crate::{Button, Key};

// ── named keys ──────────────────────────────────────────────────────────────
pub const KEY_ENTER: u16 = 28;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_ESC: u16 = 1;
pub const KEY_SPACE: u16 = 57;
pub const KEY_INSERT: u16 = 110;
pub const KEY_DELETE: u16 = 111;
pub const KEY_HOME: u16 = 102;
pub const KEY_END: u16 = 107;
pub const KEY_PAGEUP: u16 = 104;
pub const KEY_PAGEDOWN: u16 = 109;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_UP: u16 = 103;
pub const KEY_DOWN: u16 = 108;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

// F1..F10 = 59..68 (58 + n); F11 = 87, F12 = 88 (NUMLOCK/SCROLLLOCK sit at 69/70).
pub const KEY_F1: u16 = 59;
pub const KEY_F10: u16 = 68;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;

// ── US-layout symbol keys (the unshifted/shifted glyph each key produces) ────
pub const KEY_MINUS: u16 = 12; // - _
pub const KEY_EQUAL: u16 = 13; // = +
pub const KEY_LEFTBRACE: u16 = 26; // [ {
pub const KEY_RIGHTBRACE: u16 = 27; // ] }
pub const KEY_SEMICOLON: u16 = 39; // ; :
pub const KEY_APOSTROPHE: u16 = 40; // ' "
pub const KEY_GRAVE: u16 = 41; // ` ~
pub const KEY_BACKSLASH: u16 = 43; // \ |
pub const KEY_COMMA: u16 = 51; // , <
pub const KEY_DOT: u16 = 52; // . >
pub const KEY_SLASH: u16 = 53; // / ?

// ── mouse buttons (BTN_*) ────────────────────────────────────────────────────
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

/// Map a logical [`Key`] to its Linux `KEY_*` code, if the backend can emit it.
///
/// Letters/digits ignore case at the code level: uppercase is produced by the
/// caller holding a Shift modifier, not by a different code (true to evdev). Any
/// character that has no `KEY_*` (symbols, non-ASCII) maps to `None` — the uinput
/// backend then reports `NotSupported` for it (use libei for arbitrary unicode).
pub fn key_to_code(key: Key) -> Option<u16> {
    match key {
        Key::Char(c) => char_to_code(c),
        Key::Enter => Some(KEY_ENTER),
        Key::Backspace => Some(KEY_BACKSPACE),
        Key::Tab => Some(KEY_TAB),
        Key::Escape => Some(KEY_ESC),
        Key::Space => Some(KEY_SPACE),
        Key::Insert => Some(KEY_INSERT),
        Key::Delete => Some(KEY_DELETE),
        Key::Home => Some(KEY_HOME),
        Key::End => Some(KEY_END),
        Key::PageUp => Some(KEY_PAGEUP),
        Key::PageDown => Some(KEY_PAGEDOWN),
        Key::Left => Some(KEY_LEFT),
        Key::Right => Some(KEY_RIGHT),
        Key::Up => Some(KEY_UP),
        Key::Down => Some(KEY_DOWN),
        Key::LeftShift => Some(KEY_LEFTSHIFT),
        Key::RightShift => Some(KEY_RIGHTSHIFT),
        Key::LeftControl => Some(KEY_LEFTCTRL),
        Key::RightControl => Some(KEY_RIGHTCTRL),
        Key::LeftAlt => Some(KEY_LEFTALT),
        Key::RightAlt => Some(KEY_RIGHTALT),
        Key::LeftSuper => Some(KEY_LEFTMETA),
        Key::RightSuper => Some(KEY_RIGHTMETA),
        Key::F(n) => match n {
            1..=10 => Some(KEY_F1 - 1 + n as u16),
            11 => Some(KEY_F11),
            12 => Some(KEY_F12),
            _ => None,
        },
    }
}

/// Map a [`Button`] to its Linux `BTN_*` code.
pub fn button_to_code(button: Button) -> Option<u16> {
    match button {
        Button::Left => Some(BTN_LEFT),
        Button::Right => Some(BTN_RIGHT),
        Button::Middle => Some(BTN_MIDDLE),
        // uinput back/forward slots would be BTN_SIDE/BTN_EXTRA; left as future.
        Button::Other(_) => None,
    }
}

/// ASCII printable → `KEY_*`. Letters (both cases) → letter scan codes; digits →
/// digit codes; space; everything else (symbols, non-ASCII) → `None`.
fn char_to_code(c: char) -> Option<u16> {
    if c.is_ascii_alphabetic() {
        // a-z map to scan codes in QWERTY order (input-event-codes.h).
        Some(match c.to_ascii_lowercase() {
            'a' => 30,
            'b' => 48,
            'c' => 46,
            'd' => 32,
            'e' => 18,
            'f' => 33,
            'g' => 34,
            'h' => 35,
            'i' => 23,
            'j' => 36,
            'k' => 37,
            'l' => 38,
            'm' => 50,
            'n' => 49,
            'o' => 24,
            'p' => 25,
            'q' => 16,
            'r' => 19,
            's' => 31,
            't' => 20,
            'u' => 22,
            'v' => 47,
            'w' => 17,
            'x' => 45,
            'y' => 21,
            'z' => 44,
            _ => return None,
        })
    } else if c.is_ascii_digit() {
        // '1'..='9' → 2..=10; '0' → 11.
        Some(match c {
            '0' => 11,
            d => 1 + (d as u16 - b'0' as u16),
        })
    } else if c == ' ' {
        Some(KEY_SPACE)
    } else {
        None
    }
}

/// A `type_text` character resolved to its `(KEY_* scan code, needs-shift)` pair.
///
/// This is the *text* mapping — case- and shift-aware — used by
/// [`UinputSink::type_text`](crate::UinputSink::type_text). It covers the whole
/// printable ASCII set: letters (shift iff uppercase), digits, the US-layout
/// symbol keys (unshifted/shifted pairs), and space. Anything with no key
/// (non-ASCII, control chars) maps to `None`, which `type_text` reports as
/// `NotSupported`.
///
/// For the *physical key* mapping used by press/release (letters/digits/space,
/// case-insensitive, no shift), see [`char_to_code`].
pub fn char_to_key(c: char) -> Option<(u16, bool)> {
    // Letters, digits, and space reuse the physical scan code; shift is needed
    // only for uppercase letters (digits/space are never uppercase).
    if let Some(code) = char_to_code(c) {
        return Some((code, c.is_ascii_uppercase()));
    }
    // US-layout symbol pairs: each physical key yields an unshifted and a
    // shifted glyph. Shifted digits share the digit scan codes (!@#$%^&*()).
    let (code, shift) = match c {
        '-' => (KEY_MINUS, false),
        '_' => (KEY_MINUS, true),
        '=' => (KEY_EQUAL, false),
        '+' => (KEY_EQUAL, true),
        '[' => (KEY_LEFTBRACE, false),
        '{' => (KEY_LEFTBRACE, true),
        ']' => (KEY_RIGHTBRACE, false),
        '}' => (KEY_RIGHTBRACE, true),
        '\\' => (KEY_BACKSLASH, false),
        '|' => (KEY_BACKSLASH, true),
        ';' => (KEY_SEMICOLON, false),
        ':' => (KEY_SEMICOLON, true),
        '\'' => (KEY_APOSTROPHE, false),
        '"' => (KEY_APOSTROPHE, true),
        '`' => (KEY_GRAVE, false),
        '~' => (KEY_GRAVE, true),
        ',' => (KEY_COMMA, false),
        '<' => (KEY_COMMA, true),
        '.' => (KEY_DOT, false),
        '>' => (KEY_DOT, true),
        '/' => (KEY_SLASH, false),
        '?' => (KEY_SLASH, true),
        _ => return shifted_digit(c),
    };
    Some((code, shift))
}

/// `!@#$%^&*()` → the `(digit scan code, shift)` for the digit key they share.
fn shifted_digit(c: char) -> Option<(u16, bool)> {
    let digit = match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        _ => return None,
    };
    char_to_code(digit).map(|code| (code, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_canonical_codes() {
        assert_eq!(key_to_code(Key::Enter), Some(KEY_ENTER));
        assert_eq!(key_to_code(Key::Space), Some(57));
        assert_eq!(key_to_code(Key::LeftControl), Some(29));
        assert_eq!(key_to_code(Key::LeftSuper), Some(125));
    }

    #[test]
    fn function_keys_are_58_plus_n() {
        assert_eq!(key_to_code(Key::F(1)), Some(59));
        assert_eq!(key_to_code(Key::F(12)), Some(88));
        assert_eq!(key_to_code(Key::F(0)), None);
        assert_eq!(key_to_code(Key::F(13)), None);
    }

    #[test]
    fn letters_map_case_insensitively() {
        assert_eq!(key_to_code(Key::Char('a')), Some(30));
        assert_eq!(key_to_code(Key::Char('A')), Some(30)); // case lost at code level
        assert_eq!(key_to_code(Key::Char('Q')), Some(16));
        assert_eq!(key_to_code(Key::Char('z')), Some(44));
    }

    #[test]
    fn digits_and_space_map() {
        assert_eq!(key_to_code(Key::Char('1')), Some(2));
        assert_eq!(key_to_code(Key::Char('9')), Some(10));
        assert_eq!(key_to_code(Key::Char('0')), Some(11));
    }

    #[test]
    fn unmappables_return_none() {
        assert_eq!(key_to_code(Key::Char('!')), None); // symbol — no *physical* key
        assert_eq!(key_to_code(Key::Char('中')), None); // non-ASCII
        assert_eq!(key_to_code(Key::Char('\n')), None);
    }

    #[test]
    fn char_to_key_letters_and_digits() {
        assert_eq!(char_to_key('a'), Some((30, false)));
        assert_eq!(char_to_key('A'), Some((30, true))); // uppercase → shift
        assert_eq!(char_to_key('Z'), Some((44, true)));
        assert_eq!(char_to_key('7'), Some((8, false)));
        assert_eq!(char_to_key('0'), Some((11, false)));
        assert_eq!(char_to_key(' '), Some((KEY_SPACE, false)));
    }

    #[test]
    fn char_to_key_symbol_pairs() {
        // The original gap: underscore now maps (Shift + KEY_MINUS).
        assert_eq!(char_to_key('_'), Some((KEY_MINUS, true)));
        assert_eq!(char_to_key('-'), Some((KEY_MINUS, false)));
        assert_eq!(char_to_key('.'), Some((KEY_DOT, false)));
        assert_eq!(char_to_key('>'), Some((KEY_DOT, true)));
        assert_eq!(char_to_key('/'), Some((KEY_SLASH, false)));
        assert_eq!(char_to_key('?'), Some((KEY_SLASH, true)));
        assert_eq!(char_to_key('{'), Some((KEY_LEFTBRACE, true)));
    }

    #[test]
    fn char_to_key_shifted_digits() {
        assert_eq!(char_to_key('!'), Some((2, true))); // Shift+'1'
        assert_eq!(char_to_key('@'), Some((3, true))); // Shift+'2'
        assert_eq!(char_to_key('('), Some((10, true))); // Shift+'9'
        assert_eq!(char_to_key(')'), Some((11, true))); // Shift+'0'
    }

    #[test]
    fn char_to_key_unmappables_are_none() {
        assert_eq!(char_to_key('中'), None); // non-ASCII
        assert_eq!(char_to_key('\n'), None); // control char
        assert_eq!(char_to_key('\t'), None); // tab is a named key, not a glyph here
    }

    #[test]
    fn buttons_map() {
        assert_eq!(button_to_code(Button::Left), Some(0x110));
        assert_eq!(button_to_code(Button::Right), Some(0x111));
        assert_eq!(button_to_code(Button::Middle), Some(0x112));
        assert_eq!(button_to_code(Button::Other(4)), None);
    }
}
