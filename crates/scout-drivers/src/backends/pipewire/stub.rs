//! Default (feature-off) stub for [`PipeWireSource`]. See crate docs.

use crate::{CaptureSource, DriverError, Frame, Result};

/// Placeholder builder. Real options (cursor mode, source type, restore token)
/// land with the `pipewire` feature.
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct PipeWireSourceBuilder {
    _priv: (),
}

impl PipeWireSourceBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn build(&self) -> PipeWireSource {
        PipeWireSource { _priv: () }
    }
}

/// A PipeWire-backed [`CaptureSource`]. **Stub without the `pipewire` feature.**
pub struct PipeWireSource {
    #[allow(dead_code)]
    _priv: (),
}

impl PipeWireSource {
    #[must_use]
    pub fn new() -> Self {
        Self { _priv: () }
    }
}

impl Default for PipeWireSource {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureSource for PipeWireSource {
    fn size(&self) -> Option<(u32, u32)> {
        None
    }
    fn capture(&mut self) -> Result<Frame> {
        Err(DriverError::NotBuilt(
            "pipewire backend (install libpipewire-0.3-dev + enable the `pipewire` cargo feature)",
        ))
    }
}
