//! Platform backends for the driver traits.
//!
//! Most backends are gated by a cargo feature (off by default), so this crate
//! builds + tests with no native system libraries. With its feature off, a
//! backend compiles as a [`DriverError::NotBuilt`](crate::DriverError) stub;
//! with it on, the real backend links its native deps.
//!
//! | backend | trait | feature | native deps |
//! |---|---|---|---|
//! | [`pipewire`] | [`CaptureSource`](crate::CaptureSource) + [`AudioSource`](crate::AudioSource) | `pipewire` | libpipewire + xdg-desktop-portal |
//! | [`media`] | [`CaptureSource`](crate::CaptureSource) + [`AudioSource`](crate::AudioSource) | — (always on) | `ffmpeg` / `ffprobe` binary at runtime |
//! | [`uinput`] | [`InputSink`](crate::InputSink) | `uinput` | evdev (`/dev/uinput`) |
//! | [`libei`] | [`InputSink`](crate::InputSink) | `libei` | libei (not packaged; stub) |
//!
//! These used to be three standalone crates (`vrover-pipewire` / `-uinput` /
//! `-libei`); they are one crate now — a backend is just a module.
//! [`media`] is the file-backed mock (no hardware): always compiled (it has no
//! native *build* deps — it spawns `ffmpeg` as a subprocess at runtime).

pub mod libei;
pub mod media;
pub mod pipewire;
pub mod uinput;
