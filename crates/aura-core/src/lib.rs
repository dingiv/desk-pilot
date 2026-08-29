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

// ── Stage1 → Stage2 data contract · 边界范式（VadSentence / VadParagraph）──────────────
// 设计: docs/aura/stages.md（2026-08-17 重构,替代旧的 Utterance/Stage1Action
// "就地修改"契约;自 aura-asr 并入,2026-08-18）。两个时间参数切出两级实体:
//   · VAD 间隔 (vad.min_silence)  → VadSentence  原子录音片段(句级流式会话 + 句级 batch)
//   · merge 段落 (vad.merge_gap)  → VadParagraph   多句组合(定稿单位,拼接 PCM 重跑 batch)
// PCM 由 [`audio_store::AudioStore`] 按 id 持有,实体只持 id——录音数据不随事件克隆。
// 事件 append-only + 边界标记: Batch(每句)驱动 Stage2 联合整流当前段落,ParagraphEdge
// (段落关闭)驱动定稿。batch 失败显式建模为 `Option`(远程网络可能出问题)。

/// Audio clip id — assigned by [`audio_store::AudioStore`]. Entities hold ids, never PCM.
pub type AudioId = u64;
/// Sentence id — monotonic within a pipeline run.
pub type SentenceId = u64;
/// Paragraph id — monotonic within a run, assigned when the paragraph OPENS (its first SOS), so
/// live `StreamFragment` partials can carry the real id (no prospective guessing).
pub type ParagraphId = u64;

/// One VAD-gap-delimited clip — the atomic Stage1 unit. A sentence is complete the moment its
/// EOS fires: streaming session finalized, PCM inserted into the AudioStore, one batch pass
/// packed in. `batch_text: None` is LEGAL — batch depends on the remote network and may fail;
/// consumers fall back to `streaming_text` via [`VadSentence::best_text`].
#[derive(Debug, Clone)]
pub struct VadSentence {
    pub id: SentenceId,
    /// The clip's PCM, owned by the [`audio_store::AudioStore`] — never cloned into events.
    pub audio_id: AudioId,
    /// Wall-clock seconds since executor start (SOS).
    pub start_s: f64,
    /// Wall-clock seconds since executor start (EOS).
    pub end_s: f64,
    /// Per-sentence streaming ASR final (hotword-biased; the session spans exactly this sentence).
    pub streaming_text: String,
    /// Per-sentence batch ASR result. `None` when the batch pass failed (network error) or
    /// returned empty text — HttpAsr's `Err` and OnnxAsr's empty string map to the same None.
    pub batch_text: Option<String>,
}

impl VadSentence {
    /// Best available text: `batch_text` when Some(non-empty), else `streaming_text`.
    pub fn best_text(&self) -> &str {
        self.batch_text
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&self.streaming_text)
    }
}

/// A merge-paragraph composition of [`VadSentence`]s — the settle/final unit. Built when a big
/// gap (≥ `merge_gap_s`) or the settle-timeout closes the paragraph; carries a snapshot of its
/// sentences plus the paragraph-level aggregation:
/// - `streaming_text` = concat of the sentences' streaming finals (zero cost, no re-run);
/// - `batch_text` = ONE re-run of the batch model over the concatenated PCM (cross-sentence
///   context; the authoritative text Stage2 finalizes on). `None` on a failed re-run.
#[derive(Debug, Clone)]
pub struct VadParagraph {
    pub id: ParagraphId,
    /// Settle-time snapshot (ids/timestamps/texts only — no PCM per sentence).
    pub sentences: Vec<VadSentence>,
    /// SOS of the FIRST sentence.
    pub start_s: f64,
    /// EOS of the LAST sentence.
    pub end_s: f64,
    pub streaming_text: String,
    pub batch_text: Option<String>,
    /// The whole paragraph's concatenated PCM — assembled once at settle, shared (Arc) between
    /// the paragraph-level batch pass and downstream archival. The AudioStore evicts the
    /// per-sentence clips right after; this Arc is the only remaining copy.
    pub pcm: std::sync::Arc<Vec<i16>>,
    /// batch 模型调用 wall-clock 毫秒数;单句段落复用句级 batch 时为 0(不计时)。
    /// 用于性能评估:跨 ASR 后端 / 跨音频长度 / GPU vs CPU 对比。
    pub batch_asr_ms: u64,
}

impl VadParagraph {
    /// The authoritative text Stage2 finalizes on: the paragraph-level batch re-run when present,
    /// else the concat of the sentences' own best texts (per-sentence batches may have succeeded
    /// even when the paragraph re-run failed).
    pub fn best_text(&self) -> std::borrow::Cow<'_, str> {
        if let Some(t) = self.batch_text.as_deref().filter(|t| !t.trim().is_empty()) {
            return std::borrow::Cow::Borrowed(t);
        }
        std::borrow::Cow::Owned(
            self.sentences.iter().map(|s| s.best_text()).collect::<Vec<_>>().join(""),
        )
    }

    /// Paragraph duration in milliseconds (from the PCM the batch actually heard).
    pub fn duration_ms(&self) -> f32 {
        self.pcm.len() as f32 / 16_000.0 * 1000.0
    }
}

/// Events emitted by [`recognizer::Stage1Recognizer`]. Defined here (ungated) so downstream
/// crates can match on them without the `asr` feature. Append-only — consumers never mutate
/// an earlier entity in place (the old paradigm's same-seq update is gone).
#[derive(Debug, Clone)]
pub enum Stage1Event {
    /// Live streaming output for the CURRENT sentence (per-sentence session ⇒ the fragment
    /// belongs to exactly one sentence). Carries the real `paragraph_id` (assigned at the
    /// paragraph's first SOS) + `sentence_id`. Passes straight through to the UI — NOT a Stage2
    /// input (D2: no live-partial calibration). Emitted on every streaming decode change, plus
    /// one FINAL fragment at EOS carrying the sentence's definitive `streaming_text`.
    StreamFragment { paragraph_id: ParagraphId, sentence_id: SentenceId, text: String, at_s: f64 },
    /// A VAD gap closed a sentence: its batch pass is packed in. `sentences` is ALL sentences
    /// of the current paragraph so far (Stage2 jointly calibrates them — the payload IS the
    /// paragraph, keeping Stage2 stateless). Provisional until the `ParagraphEdge`.
    Batch { paragraph_id: ParagraphId, sentences: Vec<VadSentence> },
    /// The merge paragraph closed (big gap or settle-timeout): the paragraph-level batch re-run is
    /// done and packed. Authoritative — Stage2 finalizes on it; the AudioStore evicts the
    /// sentence clips right after this event.
    ParagraphEdge { paragraph: VadParagraph },
}

// ── ONNX 语音栈在 dp-models ────────────────────────────────────────────────────
// VAD (Silero) + 流式 ASR (Zipformer) + batch ASR (SenseVoice/…) 的 sherpa-onnx 封装
// 在 `dp_models::onnx`(feature `speech`),由本 crate 的 `asr` feature 转发开启。
// VAD 数据契约 + ASR provider trait 经 dp_models re-export(非 speech 门控,根部可用)。
pub use dp_models::{VadEvent, VadEventKind};
/// The ASR provider abstraction (local OnnxAsr / remote HttpAsr both impl it).
pub use dp_models::AsrProvider as Asr;
// Stage2 LLM 走 dp-router(OpenAI 兼容)。Calibrator 持 HttpLlm, 需要 trait 在 scope。
use dp_models::LlmProvider;

// ── Calibrator (aura 的 Stage2 LLM 封装层) ─────────────────────────────
// Stage2 走 dp-router(OpenAI 兼容 HTTP, 见 apps/dp-router)。`Calibrator` 是 aura 自己的
// 封装层: 持有 `dp_models::http::HttpLlm`(连到 dp-router), 附加 Stage2 的 prompt 组装
// (`calibrate_blocking`), 保持 `audio_aura_core::Calibrator` 的 API 稳定
// (native crate / examples 直接用)。
pub struct Calibrator {
    inner: dp_models::http::HttpLlm,
}

impl Calibrator {
    /// 连接到 dp-router(或任意 OpenAI 兼容上游)。`endpoint` 是 base URL(不带 /v1);
    /// `model` 是服务端模型名。
    pub fn load(endpoint: &str, model: &str) -> anyhow::Result<Self> {
        Ok(Self { inner: dp_models::http::HttpLlm::new(endpoint, model) })
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
        self.inner.complete(system, user)
    }
}

impl dp_models::LlmProvider for Calibrator {
    fn complete(&self, system: &str, user: &str) -> anyhow::Result<String> {
        self.inner.complete(system, user)
    }
}
