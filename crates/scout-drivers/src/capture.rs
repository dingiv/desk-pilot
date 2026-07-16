//! [`CaptureSource`] — the screen-capture half of the driver layer.

use crate::error::Result;
use crate::frame::Frame;

/// A source of screen frames.
///
/// Implementations: PipeWire ScreenCast (Wayland), DXGI Desktop Duplication
/// (Windows, future), v4l2 / capture card (future), `adb screencap` (Android,
/// future). Independent from [`crate::InputSink`]: a dual-machine setup may pair a
/// capture-card [`CaptureSource`] with a separate (network/HID) [`crate::InputSink`].
pub trait CaptureSource {
    /// Geometry of the source if known before the first capture; `None` if it is
    /// only discoverable by capturing (e.g. a PipeWire stream whose size arrives
    /// with the first buffer).
    fn size(&self) -> Option<(u32, u32)> {
        None
    }

    /// Grab the current frame as raw BGRA + dimensions.
    fn capture(&mut self) -> Result<Frame>;

    /// Pause (`false`) / resume (`true`) the underlying capture. Default is a
    /// no-op; real backends override it to stop the producer when idle (→ ~zero
    /// capture cost). The daemon pauses a source while nobody is pulling frames.
    fn set_active(&self, _active: bool) {}

    /// Drop the cached latest frame so the next [`CaptureSource::capture`] blocks
    /// for a fresh one (used after resuming an idle-paused source, so a caller
    /// never gets a stale frozen frame). Default is a no-op.
    fn clear_frame(&self) {}
}
