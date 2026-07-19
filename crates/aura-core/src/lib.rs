//! audio-aura-core — the **composer** (组装车间). Wires Stage1 (`audio-aura-asr`) →
//! Stage2 (`audio-aura-router`) into a [`Pipeline`], emitting [`composer::TurnEvent`]s.
//! Pure orchestration — no printing / files / Stage3 here (those are the caller's job).
//!
//! Gated behind the `asr` feature (needs the ONNX Stage1 executor).

#[cfg(feature = "asr")]
pub mod composer;

#[cfg(feature = "asr")]
pub use composer::{Pipeline, TurnEvent};
