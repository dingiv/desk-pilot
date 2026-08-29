//! view.rs — the wire contract between the aura-daemon and any client (web UI, this crate's
//! [`crate::client::AuraClient`] SDK, or the desktop-pet). Defined in the consumer-facing crate
//! (audio-aura-agent) so server + Rust clients share one type — no drift — without pulling
//! aura-core's mistralrs/asr machinery.
//!
//! Two planes:
//! - **Control plane** — [`AuraStateView`] (settings snapshot) served at `GET /api/state`;
//!   `GET /api/stream` pings `state_changed` (throttled ≥250ms) so clients re-GET. Low-frequency
//!   state: connection, config, hotwords, corrections.
//! - **Data plane** — [`AsrEvent`] pushed at `GET /api/asr_stream` (every event, low-latency):
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

/// One **data-plane** recognition event pushed by `GET /api/asr_stream` /
/// [`crate::client::AuraClient::subscribe_events`]. Serialized as `{type: "...", ...}` via the
/// internally-tagged `type` field. **Wire tags/field names are FROZEN** (`batch_segment`,
/// `window_id`, … — the old segment/window vocabulary): the prebuilt web SPA and existing day
/// logs consume them; the Rust-side rename (sentence/paragraph) is serde-renamed back. The
/// client builds + maintains its paragraph list from this stream (boundary paradigm — events
/// are append-only; a paragraph's calibration is REPLACED by the next `segment_calibration` /
/// `window_calibration` for the same paragraph, never mutated in place by unrelated events).
///
/// Five recognition events, one per producer:
/// - Stage1 streaming → [`StreamFragment`](Self::StreamFragment) (live partial + per-sentence final);
/// - Stage1 per-sentence batch → [`BatchSentence`](Self::BatchSentence);
/// - Stage1 whole-paragraph batch re-run → [`BatchParagraph`](Self::BatchParagraph);
/// - Stage2 joint calibration (per `Batch`) → [`SentenceCalibration`](Self::SentenceCalibration);
/// - Stage2 paragraph final (per `ParagraphEdge`) → [`ParagraphCalibration`](Self::ParagraphCalibration).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AsrEvent {
    /// Live streaming output for the CURRENT sentence (raw, evolving — forward correction as
    /// more audio arrives). Keyed by the real `paragraph_id` (assigned at the paragraph's first
    /// SOS) and `sentence_id` (the per-sentence streaming session's own id). Emitted on every
    /// streaming decode change, plus one FINAL fragment at EOS with the sentence's definitive
    /// text.
    StreamFragment {
        #[serde(rename = "window_id")]
        paragraph_id: u64,
        #[serde(rename = "segment_id")]
        sentence_id: u64,
        text: String,
        at_s: f64,
    },
    /// Stage1's per-sentence batch pass for the just-closed sentence (at its EOS).
    #[serde(rename = "batch_segment")]
    BatchSentence {
        #[serde(rename = "window_id")]
        paragraph_id: u64,
        #[serde(rename = "segment_id")]
        sentence_id: u64,
        text: String,
    },
    /// Stage1's whole-paragraph batch re-run (per `ParagraphEdge`) — the authoritative raw_text.
    #[serde(rename = "batch_window")]
    BatchParagraph {
        #[serde(rename = "window_id")]
        paragraph_id: u64,
        text: String,
    },
    /// Stage2 provisional JOINT calibration of the current paragraph (one per `Batch` event —
    /// i.e. per VAD gap): the calibrated text of ALL the paragraph's sentences so far,
    /// replacing the paragraph's previous calibration.
    #[serde(rename = "segment_calibration")]
    SentenceCalibration {
        #[serde(rename = "window_id")]
        paragraph_id: u64,
        calibrated: String,
    },
    /// The settled paragraph's final calibration (per `ParagraphEdge`) — the paragraph's LAST
    /// joint calibration (attached at the boundary; NO extra LLM run — the final Batch already
    /// calibrated the whole paragraph). Equals the last `segment_calibration` for this
    /// paragraph; semantically a "paragraph closed" marker carrying the final text.
    #[serde(rename = "window_calibration")]
    ParagraphCalibration {
        #[serde(rename = "window_id")]
        paragraph_id: u64,
        calibrated: String,
    },
    /// A user correction was submitted for paragraph `paragraph_id` (POST /api/correct) —
    /// mark it corrected. (The raw→corrected pair also enters the snapshot's `corrections`
    /// list as Stage2 feedback.)
    Correction {
        #[serde(rename = "window_id")]
        paragraph_id: u64,
        raw: String,
        corrected: String,
    },
}
