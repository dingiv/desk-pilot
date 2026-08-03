//! AsrBuffer — thread-safe voice recognition buffer shared between the
//! background SSE client (producer) and the keyEvent path (consumer).
//!
//! ## Thread safety
//!
//! The SSE client writes from a background tokio thread; the IME key-event
//! path reads from the fcitx5 main thread. A std::sync::Mutex serializes
//! both — the lock is held only long enough to copy a String (microseconds).
//!
//! ## Usage
//!
//! ```ignore
//! let buf = AsrBuffer::new();
//! // Producer (background SSE thread):
//! buf.update("今天天气不错");
//! // Consumer (IME keyEvent path):
//! let text = buf.snapshot(); // "今天天气不错"
//! // One-shot consume (clears after read):
//! let text = buf.take();     // Some("今天天气不错")
//! let text = buf.take();     // None (already consumed)
//! ```

use std::sync::Mutex;

/// Shared voice-recognition buffer. The inner [`String`] holds the most
/// recent calibrated final text from the aura daemon SSE stream.
pub struct AsrBuffer {
    inner: Mutex<String>,
}

impl AsrBuffer {
    /// Create an empty buffer.
    pub fn new() -> Self {
        AsrBuffer { inner: Mutex::new(String::new()) }
    }

    /// Replace the buffer with `text` (called by the SSE client on each
    /// `final` event). The previous content is discarded.
    pub fn update(&self, text: &str) {
        let mut guard = self.inner.lock().unwrap();
        *guard = text.to_string();
    }

    /// Return a clone of the current buffer without clearing it.
    /// Returns an empty string if nothing has been received yet.
    pub fn snapshot(&self) -> String {
        self.inner.lock().unwrap().clone()
    }

    /// Take the current buffer content, leaving it empty.
    /// This is a one-shot consume — the next call returns `None` until
    /// the SSE client pushes a new final.
    pub fn take(&self) -> Option<String> {
        let mut guard = self.inner.lock().unwrap();
        if guard.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *guard))
        }
    }
}

impl Default for AsrBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let buf = AsrBuffer::new();
        assert_eq!(buf.snapshot(), "");
        assert_eq!(buf.take(), None);
    }

    #[test]
    fn update_then_snapshot() {
        let buf = AsrBuffer::new();
        buf.update("你好世界");
        assert_eq!(buf.snapshot(), "你好世界");
        // snapshot doesn't consume.
        assert_eq!(buf.snapshot(), "你好世界");
    }

    #[test]
    fn take_consumes() {
        let buf = AsrBuffer::new();
        buf.update("你好世界");
        assert_eq!(buf.take(), Some("你好世界".into()));
        assert_eq!(buf.take(), None);
        assert_eq!(buf.snapshot(), "");
    }

    #[test]
    fn update_overwrites() {
        let buf = AsrBuffer::new();
        buf.update("旧文本");
        buf.update("新文本");
        assert_eq!(buf.snapshot(), "新文本");
    }
}
