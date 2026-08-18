//! view.rs — the wire contract between the aura-daemon and any client (web UI, this crate's
//! [`crate::client::AuraClient`] SDK, or the desktop-pet). Defined in the consumer-facing crate
//! (audio-aura-agent) so server + Rust clients share one type — no drift — without pulling
//! aura-core's mistralrs/asr machinery.
//!
//! Two planes:
//! - **Control plane** — [`AuraStateView`] (settings snapshot) served at `GET /api/state`;
//!   `GET /api/stream` pings `state_changed` (throttled ≥250ms) so clients re-GET. Low-frequency
//!   state: connection, config, hotwords, corrections.
//! - **Data plane** — [`AsrSegment`] pushed at `GET /api/asr_stream` (every event, low-latency):
//!   the live recognition text + per-utterance events. The client builds its utterance list from
//!   these; the snapshot carries NO utterances.

use serde::{Deserialize, Serialize};

/// Static config shown in a status panel (resolved at daemon boot from `aura.yaml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigView {
    pub asr_backend: String,
    pub asr_kind: String,
    pub asr_provider: String,
    pub llm_kind: String,
    pub model: String,
    pub vad: VadView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadView {
    pub threshold: f32,
    pub min_silence: f32,
    pub merge_gap: f64,
}

/// One user correction (raw → corrected), echoed back for a corrections panel + fed to Stage2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectionView {
    pub raw: String,
    pub corrected: String,
}

/// The **control-plane** snapshot — settings only (no utterances; recognition text lives on the
/// data plane). Served by `GET /api/state`; clients re-fetch on `state_changed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuraStateView {
    pub connected: bool,
    pub stage3_on: bool,
    pub config: ConfigView,
    pub hotwords: Vec<String>,
    pub corrections: Vec<CorrectionView>,
}

/// One **data-plane** segment pushed by `GET /api/asr_stream` /
/// [`crate::client::AuraClient::subscribe_segments`]. Serialized as `{type: "...", ...}` via the
/// internally-tagged `type` field (snake_case variant names). The client builds + maintains its
/// window list from this stream (boundary paradigm — events are append-only; a window's
/// calibration is REPLACED by the next `window_calibrated` for the same window, never mutated
/// in place by unrelated events).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AsrSegment {
    /// Live streaming partial for the CURRENT segment (raw, evolving — forward correction as
    /// more audio arrives). Keyed by the real `window_id` (assigned at the window's first SOS)
    /// + `segment_id` (the per-segment streaming session's own id).
    Interim { window_id: u64, segment_id: u64, partial: String, at_s: f64 },
    /// Stage2 provisional JOINT calibration of the current window (one per `Batch` event —
    /// i.e. per VAD gap): the calibrated text of ALL the window's segments so far, replacing
    /// the window's previous calibration.
    WindowCalibrated { window_id: u64, calibrated: String },
    /// The settled window's authoritative result (per `WindowEdge`): window-level batch text +
    /// Stage2 calibration. Window-granularity final — one per closed window.
    WindowFinal {
        window_id: u64,
        /// The window-level batch re-run (authoritative; may be empty when the re-run failed —
        /// then it fell back to the per-segment concat).
        raw_text: String,
        /// Concat of the segments' streaming finals (hotword-biased).
        streaming_text: String,
        /// The window's LAST joint calibration (attached at the boundary; NO extra LLM run —
        /// the final Batch already calibrated the whole window). Equals the last
        /// `window_calibrated` for this window.
        calibrated: String,
        route_ms: f64,
    },
    /// A user correction was submitted for window `window_id` (POST /api/correct) — mark it
    /// corrected. (The raw→corrected pair also enters the snapshot's `corrections` list as
    /// Stage2 feedback.)
    Correction { window_id: u64, raw: String, corrected: String },
}
