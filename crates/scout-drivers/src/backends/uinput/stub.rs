//! Default (feature-off) stub for [`UinputSink`]. See crate docs.

use crate::{Button, DriverError, InputSink, Key, Result};

/// Placeholder builder (real options — screen size for abs scaling — arrive with
/// the `backend` feature).
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct UinputSinkBuilder {
    _priv: (),
}

impl UinputSinkBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn build(&self) -> UinputSink {
        UinputSink::new()
    }
}

/// A uinput-backed [`InputSink`]. **Stub without the `backend` feature.**
pub struct UinputSink {
    #[allow(dead_code)]
    _priv: (),
}

impl UinputSink {
    #[must_use]
    pub fn new() -> Self {
        Self { _priv: () }
    }
}

impl Default for UinputSink {
    fn default() -> Self {
        Self::new()
    }
}

fn not_built() -> DriverError {
    DriverError::NotBuilt("uinput backend (enable the `uinput` cargo feature)")
}

impl InputSink for UinputSink {
    fn move_to(&mut self, _x: i32, _y: i32) -> Result<()> {
        Err(not_built())
    }
    fn press(&mut self, _x: i32, _y: i32, _button: Button) -> Result<()> {
        Err(not_built())
    }
    fn release(&mut self, _button: Button) -> Result<()> {
        Err(not_built())
    }
    fn scroll(&mut self, _x: i32, _y: i32, _dx: i32, _dy: i32) -> Result<()> {
        Err(not_built())
    }
    fn key_press(&mut self, _key: Key) -> Result<()> {
        Err(not_built())
    }
    fn key_release(&mut self, _key: Key) -> Result<()> {
        Err(not_built())
    }
    fn type_text(&mut self, _text: &str) -> Result<()> {
        Err(not_built())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_not_built() {
        let mut s = UinputSink::new();
        assert!(matches!(s.move_to(0, 0), Err(DriverError::NotBuilt(_))));
        assert!(matches!(s.type_text("a"), Err(DriverError::NotBuilt(_))));
    }
}
