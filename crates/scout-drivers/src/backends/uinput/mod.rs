//! `uinput` — [`InputSink`](crate::InputSink) backend via the Linux **uinput**
//! kernel virtual device.
//!
//! uinput is the most portable Linux input path: it creates a virtual keyboard +
//! mouse in `/dev/uinput` and emits raw evdev events, so it works regardless of
//! the compositor (Wayland or X11). It needs root or the `input`/`uinput` group.
//!
//! # Layout.
//! - [`keycode`]: the pure, always-compiled, unit-tested logical → Linux code map.
//! - With the `uinput` feature (default off): [`UinputSink`] drives a real evdev
//!   `VirtualDevice`. Without it, [`UinputSink`] is a [`DriverError::NotBuilt`]
//!   stub so the crate builds without pulling evdev.
//!
//! # Caveats.
//! - `type_text` covers the printable ASCII subset (letters, digits, and the
//!   US-layout symbol keys via Shift); non-ASCII returns `NotSupported` (use the
//!   libei backend for arbitrary unicode).
//! - Absolute-pointer scaling to screen geometry is approximate without a screen
//!   size hint; pass one via [`UinputSink::with_screen`] (backend feature).
//! - Live injection needs `/dev/uinput` write access (root or the `uinput` group,
//!   or `chmod 0666`) + a compositor session to route the events to the cursor.

pub mod keycode;

#[cfg(feature = "uinput")]
mod backend;
#[cfg(not(feature = "uinput"))]
mod stub;

#[cfg(feature = "uinput")]
pub use backend::{UinputSink, UinputSinkBuilder};
#[cfg(not(feature = "uinput"))]
pub use stub::{UinputSink, UinputSinkBuilder};
