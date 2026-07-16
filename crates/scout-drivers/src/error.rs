//! Driver-layer error type. One enum shared by every backend so callers match a
//! single `Result`.

use std::io;

/// Anything a [`crate::CaptureSource`] or [`crate::InputSink`] can fail with.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// The backend was compiled out — enable the matching cargo feature (and, for
    /// native deps, install the system library). E.g. the `libei` backend without
    /// the `libei` feature, or the `pipewire` backend without PipeWire headers.
    #[error("driver backend not built: {0} (enable the matching cargo feature / system library)")]
    NotBuilt(&'static str),

    /// The backend is present but this operation is unsupported on this target
    /// (e.g. `type_text` with non-ASCII on `uinput`, which has no keysym layer).
    #[error("operation not supported by this driver backend: {0}")]
    NotSupported(String),

    /// Bad caller input (negative size, mismatched buffer length, out-of-range arg).
    #[error("invalid argument: {0}")]
    InvalidArg(String),

    /// Low-level OS I/O (opening `/dev/uinput`, reading a PipeWire fd, …).
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// A session/portal negotiation failed (xdg-desktop-portal ScreenCast, libei
    /// RemoteDesktop handshake, …) before any capture/input could happen.
    #[error("session error: {0}")]
    Session(String),

    /// Catch-all for an underlying backend returning its own error string.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Convenience alias so backends can write `Result<Frame>` without qualifying.
pub type Result<T> = std::result::Result<T, DriverError>;
