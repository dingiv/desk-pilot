//! [`InputSink`] — the mouse + keyboard injection half of the driver layer.

use crate::control::{Button, Key};
use crate::error::Result;

/// A sink for mouse + keyboard input events.
///
/// Coordinate semantics are absolute screen pixels (matching how SoM tooling
/// resolves a mark → element bounds → center). Pointer methods take the target
/// `(x, y)` so a backend can move-then-press atomically; `click` is provided as a
/// sane default of `press` + `release`.
///
/// Implementations: `uinput` (Linux kernel virtual device), `libei`
/// (xdg-desktop-portal emulated input), Win32 `SendInput` (future), `adb input`
/// (Android, future).
pub trait InputSink {
    /// Move the pointer to absolute `(x, y)`.
    fn move_to(&mut self, x: i32, y: i32) -> Result<()>;

    /// Move to `(x, y)` and press `button` down.
    fn press(&mut self, x: i32, y: i32, button: Button) -> Result<()>;

    /// Release `button` (no movement).
    fn release(&mut self, button: Button) -> Result<()>;

    /// A full click at `(x, y)`: move → press → release. Backends rarely override.
    fn click(&mut self, x: i32, y: i32, button: Button) -> Result<()> {
        self.press(x, y, button)?;
        self.release(button)
    }

    /// Scroll at `(x, y)` by `(dx, dy)` "notches". Sign encodes direction; a
    /// typical wheel notch is `±1`. Horizontal = `dx`, vertical = `dy`.
    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()>;

    /// Press `key` down.
    fn key_press(&mut self, key: Key) -> Result<()>;

    /// Release `key`.
    fn key_release(&mut self, key: Key) -> Result<()>;

    /// Press and release `key`. Backends rarely override.
    fn tap_key(&mut self, key: Key) -> Result<()> {
        self.key_press(key)?;
        self.key_release(key)
    }

    /// Type a unicode string. Backends without a keysym/unicode layer (notably
    /// `uinput`) may map only the ASCII subset and return
    /// [`DriverError::NotSupported`][crate::DriverError::NotSupported] for the rest.
    fn type_text(&mut self, text: &str) -> Result<()>;
}
