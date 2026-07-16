//! In-process test stubs — a programmable [`CaptureSource`] and a recording
//! [`InputSink`] that never touch real hardware. Kept in the core crate (behind no
//! feature) so downstream backend crates can unit-test their own glue against
//! these without spinning up a device, and so the trait's default methods can be
//! exercised here.

use crate::capture::CaptureSource;
use crate::control::{Button, Key};
use crate::error::Result;
use crate::frame::Frame;
use crate::input::InputSink;

/// A [`CaptureSource`] that returns a solid frame every call and counts captures.
pub struct MockCaptureSource {
    width: u32,
    height: u32,
    b: u8,
    g: u8,
    r: u8,
    captures: u32,
}

impl MockCaptureSource {
    #[must_use]
    pub fn solid(width: u32, height: u32, b: u8, g: u8, r: u8) -> Self {
        Self {
            width,
            height,
            b,
            g,
            r,
            captures: 0,
        }
    }

    /// How many times [`CaptureSource::capture`] has been called.
    #[must_use]
    pub fn capture_count(&self) -> u32 {
        self.captures
    }
}

impl CaptureSource for MockCaptureSource {
    fn size(&self) -> Option<(u32, u32)> {
        Some((self.width, self.height))
    }
    fn capture(&mut self) -> Result<Frame> {
        self.captures += 1;
        Ok(Frame::solid(self.width, self.height, self.b, self.g, self.r))
    }
}

/// One recorded call against a [`RecordingInputSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedEvent {
    MoveTo(i32, i32),
    Press(i32, i32, Button),
    Release(Button),
    Scroll(i32, i32, i32, i32),
    KeyPress(Key),
    KeyRelease(Key),
    Type(String),
}

/// An [`InputSink`] that records every call into `events` instead of injecting.
#[derive(Debug, Default)]
pub struct RecordingInputSink {
    pub events: Vec<RecordedEvent>,
}

impl RecordingInputSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl InputSink for RecordingInputSink {
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.events.push(RecordedEvent::MoveTo(x, y));
        Ok(())
    }
    fn press(&mut self, x: i32, y: i32, button: Button) -> Result<()> {
        self.events.push(RecordedEvent::Press(x, y, button));
        Ok(())
    }
    fn release(&mut self, button: Button) -> Result<()> {
        self.events.push(RecordedEvent::Release(button));
        Ok(())
    }
    fn scroll(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        self.events.push(RecordedEvent::Scroll(x, y, dx, dy));
        Ok(())
    }
    fn key_press(&mut self, key: Key) -> Result<()> {
        self.events.push(RecordedEvent::KeyPress(key));
        Ok(())
    }
    fn key_release(&mut self, key: Key) -> Result<()> {
        self.events.push(RecordedEvent::KeyRelease(key));
        Ok(())
    }
    fn type_text(&mut self, text: &str) -> Result<()> {
        self.events.push(RecordedEvent::Type(text.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_capture_counts_and_returns_solid() {
        let mut c = MockCaptureSource::solid(4, 2, 1, 2, 3);
        assert_eq!(c.size(), Some((4, 2)));
        assert_eq!(c.capture_count(), 0);
        let f = c.capture().unwrap();
        assert_eq!(c.capture_count(), 1);
        assert_eq!((f.width, f.height), (4, 2));
        assert_eq!(f.bgra.len(), 4 * 2 * 4);
    }

    #[test]
    fn default_click_is_press_then_release() {
        let mut s = RecordingInputSink::new();
        s.click(10, 20, Button::Left).unwrap();
        assert_eq!(
            s.events,
            vec![
                RecordedEvent::Press(10, 20, Button::Left),
                RecordedEvent::Release(Button::Left),
            ]
        );
    }

    #[test]
    fn default_tap_key_is_press_then_release() {
        let mut s = RecordingInputSink::new();
        s.tap_key(Key::Enter).unwrap();
        assert_eq!(
            s.events,
            vec![
                RecordedEvent::KeyPress(Key::Enter),
                RecordedEvent::KeyRelease(Key::Enter),
            ]
        );
    }

    #[test]
    fn records_all_input_variants() {
        let mut s = RecordingInputSink::new();
        s.move_to(1, 2).unwrap();
        s.scroll(5, 6, -1, 2).unwrap();
        s.type_text("hi").unwrap();
        assert_eq!(
            s.events,
            vec![
                RecordedEvent::MoveTo(1, 2),
                RecordedEvent::Scroll(5, 6, -1, 2),
                RecordedEvent::Type("hi".to_owned()),
            ]
        );
    }
}
