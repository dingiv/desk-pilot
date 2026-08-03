//! view.rs — the `AuraStateView` snapshot: the authoritative wire contract between the
//! aura-daemon and any client (the web UI, this crate's [`crate::client::AuraClient`] SDK, or the
//! desktop-pet). Defined here in the consumer-facing crate (audio-aura-agent) so both the server
//! (daemon, which imports it) and Rust clients share one type — no drift — without pulling the
//! heavy mistralrs/asr machinery of aura-core.
//!
//! Snapshot-sync contract: the daemon serves the full `AuraStateView` at `GET /api/state`; on any
//! state change it bumps a `version` counter and pings subscribers via `GET /api/stream`
//! (`{type:"state_changed"}`, throttled ≥250ms). Clients re-GET `/api/state` on a ping.

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

/// One user correction (raw → corrected), echoed back for a corrections panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectionView {
    pub raw: String,
    pub corrected: String,
}

/// A finalized utterance's authoritative fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalView {
    pub raw: String,
    pub streaming: String,
    pub calibrated: String,
    pub intent: String,
    pub reply: String,
    pub route_ms: f64,
}

/// One utterance in the timeline (live or finalized).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtteranceView {
    pub seq: u64,
    /// Latest streaming partial (raw, live).
    pub partial: String,
    /// Stage2's provisional calibration (per fragment) — shown in preference to `partial` when set.
    pub calibrated: Option<String>,
    /// Set when the utterance settled (VAD settle + Stage2 final calibration). (`final` is a Rust
    /// keyword, so the field is `final_` renamed to `final` on the wire — matches the JS/JSON name.)
    #[serde(rename = "final")]
    pub final_: Option<FinalView>,
    /// Still being recognized (absorbing fragments).
    pub live: bool,
    /// Set when the user corrected this utterance via POST /api/correct.
    pub corrected_by_user: bool,
    pub at_s: f64,
}

/// The complete state snapshot. Served by the daemon's `GET /api/state`; clients re-fetch on
/// `state_changed`. `utterances` is bounded to the most recent N by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuraStateView {
    pub connected: bool,
    pub stage3_on: bool,
    pub config: ConfigView,
    pub hotwords: Vec<String>,
    pub corrections: Vec<CorrectionView>,
    pub utterances: Vec<UtteranceView>,
}
