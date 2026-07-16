//! `pipewire` — [`CaptureSource`](crate::CaptureSource) backend via **PipeWire
//! ScreenCast** (the portable Wayland screen-capture path through
//! xdg-desktop-portal).
//!
//! # Default build: stub.
//!
//! The real backend lives behind the `pipewire` cargo feature. Without it, the
//! module compiles as a documented [`DriverError::NotBuilt`](crate::DriverError)
//! stub so the crate builds without PipeWire headers. With the feature on,
//! [`PipeWireSource`] opens a ScreenCast session via `ashpd` and streams frames
//! via the `pipewire` crate.
//!
//! # Runtime needs (cannot be satisfied in this dev container without setup).
//!
//! Live capture needs a real Wayland session + a running xdg-desktop-portal —
//! the container's `WAYLAND_DISPLAY` socket is VS Code's own rendering, not a
//! capturable desktop. So this backend is **compile-verified here, run-verified
//! on a real host** (see `crates/README.md` → "PipeWire backend").

#[cfg(not(feature = "pipewire"))]
mod stub;
#[cfg(feature = "pipewire")]
mod backend;
#[cfg(feature = "pipewire")]
mod audio;

#[cfg(not(feature = "pipewire"))]
pub use stub::{PipeWireSource, PipeWireSourceBuilder};
#[cfg(feature = "pipewire")]
pub use backend::{PipeWireSource, PipeWireSourceBuilder};
#[cfg(feature = "pipewire")]
pub use audio::PipeWireAudioSource;

#[cfg(test)]
mod tests {
    #[cfg(not(feature = "pipewire"))]
    #[test]
    fn stub_reports_not_built() {
        use crate::{CaptureSource, DriverError};
        let mut s = super::PipeWireSource::new();
        assert!(matches!(s.capture(), Err(DriverError::NotBuilt(_))));
    }
}
