//! audio-aura-core — the whole audio-aura voice stack in one crate: Stage1 (ingest + VAD +
//! two-pass ASR + the boundary-paradigm contract) → Stage2 (joint calibrator + prompt) →
//! storage (audio archive + turn log + recent ring) + the TTS capability interface.
//! Merged history: aura-core (composer) + aura-dcl (calibrator/prompt) + aura-store
//! (hub/archive/wav) + **aura-asr (Stage1, 2026-08-18 并入)** + **aura-tts (占位, 并入)**.
//!
//! External dep graph: this crate → dp-models/shared; daemon → this crate (＋ aura-agent SDK).
//!
//! `recognizer` (ONNX Stage1: Silero VAD + streaming/batch ASR) and `pipeline` (the
//! [`Pipeline`]) are gated behind the `asr` feature (= `dp-models/speech`) — the default
//! build stays light (contract + calibrator + storage, no sherpa-onnx). Enable with
//! `features = ["asr"]`.

pub mod archive;
pub mod audio_store;
pub mod buffer;
pub mod calibrator;
#[cfg(feature = "asr")]
pub mod pipeline;
#[cfg(feature = "asr")]
pub mod recognizer;
pub mod hub;
pub mod prompt;
pub mod scout;
pub mod tts;
pub mod vad;
pub mod wav;

pub use calibrator::{LlmInput, PassThroughCalibrator, Stage2Calibrator, Stage2CalibratorImpl};
#[cfg(feature = "asr")]
pub use pipeline::{AsrSpec, LlmSpec, Pipeline, PipelineSpec, StreamSpec, TurnEvent, VadSpec};
pub use prompt::PromptBuilder;
pub use hub::{FinalTurn, Storage, TurnRecord};

// ── Stage1 → Stage2 data contract · 边界范式（VadSegment / VadWindow）──────────────
// 设计: docs/aura/vad-segment-model.md（2026-08-17 重构,替代旧的 Utterance/Stage1Action
// "就地修改"契约;自 aura-asr 并入,2026-08-18）。两个时间参数切出两级实体:
//   · VAD 间隔 (vad.min_silence)  → VadSegment  原子录音片段(段级流式会话 + 段级 batch)
//   · merge 窗口 (vad.merge_gap)  → VadWindow   多段组合(定稿单位,拼接 PCM 重跑 batch)
// PCM 由 [`audio_store::AudioStore`] 按 id 持有,实体只持 id——录音数据不随事件克隆。
// 事件 append-only + 边界标记: Batch(每段)驱动 Stage2 联合整流当前窗口,WindowEdge
// (窗口关闭)驱动定稿。batch 失败显式建模为 `Option`(远程网络可能出问题)。

/// Audio clip id — assigned by [`audio_store::AudioStore`]. Entities hold ids, never PCM.
pub type AudioId = u64;
/// Segment id — monotonic within a pipeline run.
pub type SegmentId = u64;
/// Window id — monotonic within a run, assigned when the window OPENS (its first SOS), so
/// live `StreamFragment` partials can carry the real id (no prospective guessing).
pub type WindowId = u64;

/// One VAD-gap-delimited clip — the atomic Stage1 unit. A segment is complete the moment its
/// EOS fires: streaming session finalized, PCM inserted into the AudioStore, one batch pass
/// packed in. `batch_text: None` is LEGAL — batch depends on the remote network and may fail;
/// consumers fall back to `streaming_text` via [`VadSegment::best_text`].
#[derive(Debug, Clone)]
pub struct VadSegment {
    pub id: SegmentId,
    /// The clip's PCM, owned by the [`audio_store::AudioStore`] — never cloned into events.
    pub audio_id: AudioId,
    /// Wall-clock seconds since executor start (SOS).
    pub start_s: f64,
    /// Wall-clock seconds since executor start (EOS).
    pub end_s: f64,
    /// Per-segment streaming ASR final (hotword-biased; the session spans exactly this segment).
    pub streaming_text: String,
    /// Per-segment batch ASR result. `None` when the batch pass failed (network error) or
    /// returned empty text — HttpAsr's `Err` and OnnxAsr's empty string map to the same None.
    pub batch_text: Option<String>,
}

impl VadSegment {
    /// Best available text: `batch_text` when Some(non-empty), else `streaming_text`.
    pub fn best_text(&self) -> &str {
        self.batch_text
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&self.streaming_text)
    }
}

/// A merge-window composition of [`VadSegment`]s — the settle/final unit. Built when a big
/// gap (≥ `merge_gap_s`) or the settle-timeout closes the window; carries a snapshot of its
/// segments plus the window-level aggregation:
/// - `streaming_text` = concat of the segments' streaming finals (zero cost, no re-run);
/// - `batch_text` = ONE re-run of the batch model over the concatenated PCM (cross-segment
///   context; the authoritative text Stage2 finalizes on). `None` on a failed re-run.
#[derive(Debug, Clone)]
pub struct VadWindow {
    pub id: WindowId,
    /// Settle-time snapshot (ids/timestamps/texts only — no PCM per segment).
    pub segments: Vec<VadSegment>,
    /// SOS of the FIRST segment.
    pub start_s: f64,
    /// EOS of the LAST segment.
    pub end_s: f64,
    pub streaming_text: String,
    pub batch_text: Option<String>,
    /// The whole window's concatenated PCM — assembled once at settle, shared (Arc) between
    /// the window-level batch pass and downstream archival. The AudioStore evicts the
    /// per-segment clips right after; this Arc is the only remaining copy.
    pub pcm: std::sync::Arc<Vec<i16>>,
}

impl VadWindow {
    /// The authoritative text Stage2 finalizes on: the window-level batch re-run when present,
    /// else the concat of the segments' own best texts (per-segment batches may have succeeded
    /// even when the window re-run failed).
    pub fn best_text(&self) -> std::borrow::Cow<'_, str> {
        if let Some(t) = self.batch_text.as_deref().filter(|t| !t.trim().is_empty()) {
            return std::borrow::Cow::Borrowed(t);
        }
        std::borrow::Cow::Owned(
            self.segments.iter().map(|s| s.best_text()).collect::<Vec<_>>().join(""),
        )
    }

    /// Window duration in milliseconds (from the PCM the batch actually heard).
    pub fn duration_ms(&self) -> f32 {
        self.pcm.len() as f32 / 16_000.0 * 1000.0
    }
}

/// Events emitted by [`recognizer::Stage1Recognizer`]. Defined here (ungated) so downstream
/// crates can match on them without the `asr` feature. Append-only — consumers never mutate
/// an earlier entity in place (the old paradigm's same-seq update is gone).
#[derive(Debug, Clone)]
pub enum Stage1Event {
    /// Live streaming output for the CURRENT segment (per-segment session ⇒ the fragment
    /// belongs to exactly one segment). Carries the real `window_id` (assigned at the
    /// window's first SOS) + `segment_id`. Passes straight through to the UI — NOT a Stage2
    /// input (D2: no live-partial calibration). Emitted on every streaming decode change, plus
    /// one FINAL fragment at EOS carrying the segment's definitive `streaming_text`.
    StreamFragment { window_id: WindowId, segment_id: SegmentId, text: String, at_s: f64 },
    /// A VAD gap closed a segment: its batch pass is packed in. `segments` is ALL segments
    /// of the current window so far (Stage2 jointly calibrates them — the payload IS the
    /// window, keeping Stage2 stateless). Provisional until the `WindowEdge`.
    Batch { window_id: WindowId, segments: Vec<VadSegment> },
    /// The merge window closed (big gap or settle-timeout): the window-level batch re-run is
    /// done and packed. Authoritative — Stage2 finalizes on it; the AudioStore evicts the
    /// segment clips right after this event.
    WindowEdge { window: VadWindow },
}

// ── ONNX 语音栈在 dp-models ────────────────────────────────────────────────────
// VAD (Silero) + 流式 ASR (Zipformer) + batch ASR (SenseVoice/…) 的 sherpa-onnx 封装
// 在 `dp_models::onnx`(feature `speech`),由本 crate 的 `asr` feature 转发开启。
// VAD 数据契约 + ASR provider trait 经 dp_models re-export(非 speech 门控,根部可用)。
pub use dp_models::{VadEvent, VadEventKind};
/// The ASR provider abstraction (local OnnxAsr / remote HttpAsr both impl it).
pub use dp_models::AsrProvider as Asr;

// ── Calibrator (aura 的 Stage2 本地 LLM 封装层) ─────────────────────────────
// 模型本体 (mistralrs GGUF 加载) 在 `dp_models::mistral::MistralLlm` —— dp-models 是通用
// 模型提供库, 命名面向通用能力 (local-mistral provider)。`Calibrator` 是 aura 自己的
// 封装层: 持有 MistralLlm, 附加 Stage2 的 prompt 组装 (calibrate_blocking), 保持
// `audio_aura_core::Calibrator` 的 API 不变 (native/examples 直接用)。
#[cfg(feature = "mistral")]
pub struct Calibrator {
    inner: dp_models::MistralLlm,
}

#[cfg(feature = "mistral")]
impl Calibrator {
    pub fn load(model_dir: &str, model_file: &str) -> anyhow::Result<Self> {
        Ok(Self { inner: dp_models::MistralLlm::load(model_dir, model_file)? })
    }

    /// Load by model file name only — the model **directory** is resolved via the shared
    /// `MODELS` namespace (declared in dp-models' `Cargo.toml`; dev = workspace
    /// `assets/models`, prod = `~/.desk-pilot/models`).
    pub fn load_default(model_file: &str) -> anyhow::Result<Self> {
        Ok(Self { inner: dp_models::MistralLlm::load_default(model_file)? })
    }

    /// Run the merged 整流+路由 on one utterance; returns the model's raw JSON text.
    pub fn calibrate_blocking(
        &self,
        raw_text: &str,
        context: Option<&str>,
        hotwords: &[String],
    ) -> anyhow::Result<String> {
        let mut pb = crate::prompt::PromptBuilder::new(raw_text).hotwords(hotwords);
        if let Some(c) = context {
            pb = pb.context(c);
        }
        let (system, user) = pb.build();
        self.infer(&system, &user)
    }

    /// Raw one-shot chat: send a (system, user) pair.
    pub fn infer(&self, system: &str, user: &str) -> anyhow::Result<String> {
        self.inner.infer(system, user)
    }
}

#[cfg(feature = "mistral")]
impl dp_models::LlmProvider for Calibrator {
    fn complete(&self, system: &str, user: &str) -> anyhow::Result<String> {
        self.inner.complete(system, user)
    }
}
