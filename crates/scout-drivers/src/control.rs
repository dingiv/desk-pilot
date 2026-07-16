//! Logical input controls shared across backends. Each backend maps these to its
//! own keycode world: Linux `KEY_*` (uinput), the `ei` event stream (libei),
//! Win32 `VK_*` (future), Android `input` keycodes (future).

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    /// An additional/extended button (back/forward etc.) by index.
    Other(u8),
}

/// A logical key. Printable characters go through [`Key::Char`]; everything else
/// is a named key. `type_text` handles arbitrary unicode on backends that support
/// a keysym/unicode layer (libei, future Win32); uinput falls back to the ASCII
/// subset it can map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character (case-significant). E.g. `Key::Char('a')`, `Key::Char('A')`.
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Space,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    // Modifiers (handed, for press/release combos).
    LeftShift,
    RightShift,
    LeftControl,
    RightControl,
    LeftAlt,
    RightAlt,
    LeftSuper,
    RightSuper,
    /// Function key F1..F12 (`F(1)`..`F(12)`).
    F(u8),
}
