//! `vrover-drivers` — the cross-platform **abstraction** layer for VRover's native
//! driver layer.
//!
//! Two independent concerns, deliberately split (see `crates/README.md`):
//! - [`CaptureSource`] — produce screen frames (`Frame`, raw BGRA). Backends:
//!   PipeWire (Wayland), DXGI (Windows, future), v4l2 (capture card, future),
//!   `adb screencap` (Android, future).
//! - [`InputSink`] — inject mouse + keyboard events. Backends: `uinput`
//!   (Linux kernel virtual device), `libei` (xdg-desktop-portal emulated input),
//!   Win32 SendInput (future), `adb input` (Android, future).
//!
//! They are separate traits because on a dual-machine / capture-card setup the
//! frame *source* and the input *sink* may target different devices. A concrete
//! backend implements one or both.
//!
//! This crate is a pure leaf: no platform crates, no system libraries. The
//! trait/enum shapes are kept aligned with the TypeScript `NativeLayer` /
//! `Platform` contract (`packages/platform/src/desktop.ts`) so a future napi-rs
//! binding can lift a `CaptureSource` + `InputSink` pair into it verbatim.

pub mod audio;
pub mod capture;
pub mod control;
pub mod error;
pub mod frame;
pub mod input;
pub mod mock;

/// Feature-gated platform backends — `pipewire` (capture), `uinput` + `libei`
/// (input). Each is an optional module behind a cargo feature (off by default),
/// so this crate still builds + tests with no native system libraries.
pub mod backends;

pub use capture::CaptureSource;
pub use control::{Button, Key};
pub use error::{DriverError, Result};
pub use frame::Frame;
pub use input::InputSink;
pub use mock::{MockCaptureSource, RecordedEvent, RecordingInputSink};

// Feature-gated backend re-exports (convenience for the napi binding crate).
#[cfg(feature = "uinput")]
pub use backends::uinput::{UinputSink, UinputSinkBuilder};
// media (file-backed mock) is always compiled — no native build deps.
pub use backends::media::{MediaAudioSource, MediaVideoSource};
