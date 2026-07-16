//! audio-aura-core library — the **composer** (组装车间). Wires Stage1 (`audio-aura-asr`) →
//! Stage2 (`audio-aura-router`) into a [`Pipeline`], emitting [`composer::TurnEvent`]s. Pure
//! orchestration — no printing / files / Stage3 here (those are the caller's job).
//!
//! The legacy binary `src/main.rs` + `ingest.rs` + `pipeline.rs` + `routes.rs` (the old axum
//! energy-VAD daemon) are **deprecated** — superseded by the `daemon` crate. They remain for now
//! and are removed once `daemon` takes over.
//!
//! The composer needs the ONNX Stage1 executor, so it is gated behind the `asr` feature.

#[cfg(feature = "asr")]
pub mod composer;

#[cfg(feature = "asr")]
pub use composer::{Pipeline, TurnEvent};
