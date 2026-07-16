//! `libei` — VRover [`InputSink`](crate::InputSink) backend via **libei** (emulated input over
//! the xdg-desktop-portal RemoteDesktop/InputCapture interfaces).
//!
//! # Status (this round): scaffold.
//!
//! libei is the compositor-correct, **no-root** way to inject input on Wayland —
//! the right long-term pairing with the PipeWire capture backend. But:
//! - `libei-dev` is **not** packaged in this dev container,
//! - the Rust bindings (`libei-rs` FFI / via libportal) are niche and lag the C lib.
//!
//! So this round the crate is a documented **stub**. The trait is wired so the
//! shape is concrete; the real FFI/portal handshake lands in a follow-up. Enable
//! the `libei` cargo feature once the system lib + bindings are in place — for now
//! it still returns [`DriverError::NotBuilt`] (the feature is reserved).
//!
//! See `crates/README.md` for how to bring up the real libei backend.

use crate::{
    DriverError, InputSink, Result,
    control::{Button, Key},
};

/// A libei-backed [`InputSink`]. **Not implemented this round** — every method
/// returns [`DriverError::NotBuilt`]. Construct with [`LibeiSink::new`].
pub struct LibeiSink {
    #[allow(dead_code)]
    _priv: (),
}

impl LibeiSink {
    /// Open a libei session. Today this always reports "not built".
    #[must_use]
    pub fn new() -> Self {
        Self { _priv: () }
    }
}

impl Default for LibeiSink {
    fn default() -> Self {
        Self::new()
    }
}

const NOT_BUILT: DriverError = DriverError::NotBuilt(
    "libei backend (install libei + the `libei` cargo feature; not packaged here yet)",
);

impl InputSink for LibeiSink {
    fn move_to(&mut self, _x: i32, _y: i32) -> Result<()> {
        Err(NOT_BUILT)
    }
    fn press(&mut self, _x: i32, _y: i32, _button: Button) -> Result<()> {
        Err(NOT_BUILT)
    }
    fn release(&mut self, _button: Button) -> Result<()> {
        Err(NOT_BUILT)
    }
    fn scroll(&mut self, _x: i32, _y: i32, _dx: i32, _dy: i32) -> Result<()> {
        Err(NOT_BUILT)
    }
    fn key_press(&mut self, _key: Key) -> Result<()> {
        Err(NOT_BUILT)
    }
    fn key_release(&mut self, _key: Key) -> Result<()> {
        Err(NOT_BUILT)
    }
    fn type_text(&mut self, _text: &str) -> Result<()> {
        Err(NOT_BUILT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_not_built() {
        let mut s = LibeiSink::new();
        assert!(s.move_to(1, 2).is_err());
        assert!(s.type_text("hi").is_err());
        // The default click() would call press+release; both error, so click errors.
        assert!(s.click(1, 2, Button::Left).is_err());
        match s.tap_key(Key::Enter) {
            Err(DriverError::NotBuilt(_)) => {}
            other => panic!("expected NotBuilt, got {other:?}"),
        }
    }
}
