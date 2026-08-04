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
/// utterance list from this stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AsrSegment {
    /// Live streaming partial (raw, evolving — forward correction as more audio arrives).
    Interim { seq: u64, partial: String, at_s: f64 },
    /// Stage2 provisional calibration of an in-progress utterance (per merged fragment).
    CalibratedInterim { seq: u64, calibrated: String },
    /// Settled utterance — authoritative batch ASR + Stage2 final calibration.
    Final {
        seq: u64,
        raw_text: String,
        streaming_text: String,
        calibrated: String,
        intent: String,
        reply: String,
        route_ms: f64,
    },
    /// A user correction was submitted for utterance `seq` (POST /api/correct) — mark it corrected.
    /// (The raw→corrected pair also enters the snapshot's `corrections` list as Stage2 feedback.)
    Correction { seq: u64, raw: String, corrected: String },
}
