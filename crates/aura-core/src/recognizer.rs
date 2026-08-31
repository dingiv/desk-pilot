//! Stage1Recognizer — encapsulates the Stage1 "noodle": the audio ring + Silero VAD +
//! per-sentence streaming sessions + the paragraph tracker. Owns ALL the consume-loop state and
//! runs the consume loop internally, emitting [`Stage1Event`]s — it does NOT touch files or run
//! Stage2 (that's `pipeline`'s job, `audio_aura_core::Pipeline`).
//!
//! **本 crate 不创建任何线程** —— 三条阻塞工作全部以函数暴露,线程由 `Pipeline` 创建并运行:
//! - [`Self::run_ingest`] — scout TCP → AudioRing(阻塞,自动重连);
//! - [`Self::run_batch_worker`] — 消费循环在 EOS/settle 经 mpsc 投递 [`BatchJob`],worker 逐个
//!   执行**阻塞**的 `AsrProvider::recognize`,每 job 必出一次 [`BatchJobResult`](Some/None);
//! - [`Self::run`] — 消费循环(VAD + 流式 + 边界决策),**永不被 batch 阻塞**
//!   (batch 异步化的根因修复:见 docs/aura/async-batch-design.md)。
//!
//! Boundary paradigm (docs/aura/stages.md): the VAD gap (`min_silence`) closes a
//! [`VadSentence`] (its own streaming session per D1 + one batch JOB enqueued, packed as a
//! `Batch` event with `batch_text: None`; the result arrives via `SentenceBatchReady`); the
//! merge paragraph (`merge_gap`) closes a [`VadParagraph`] (concatenated PCM re-run JOB
//! enqueued, packed as a `ParagraphEdge` with `batch_text: None`; the result arrives via
//! `ParagraphBatchReady`). PCM lives in the [`AudioStore`] by id as a shared `Arc<Vec<i16>>` —
//! jobs and the paragraph share the allocation; events carry ids + texts only.
//!
//! ```ignore
//! let (s1, batch_rx) = OnnxStage1Recognizer::new(Stage1Config::new(scout_addr))?;
//! let pipeline = Pipeline::new(s1, Box::new(stage2), batch_rx);
//! pipeline.run(running, resume, |ev| { /* TurnEvents */ });
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A batch job the consume loop enqueues (non-blocking, microseconds); a dedicated worker
/// thread — spawned by [`crate::pipeline::Pipeline`] — drains this channel and runs the
/// blocking [`AsrProvider::recognize`] per job. PCM is a shared `Arc` (no copy — the
/// [`AudioStore`] / sentence / paragraph all share the same allocation).
///
/// Ordering: a `Sentence` job is enqueued at the sentence's EOS (its `Batch` is emitted in the
/// same call, BEFORE the result can exist); a `Paragraph` re-run job at settle (its
/// `ParagraphEdge` likewise). Every job yields EXACTLY ONE [`BatchJobResult`] — failure maps to
/// `text: None` (the legal "batch unavailable" state), never a dropped job, so the pipeline's
/// readiness gate (all sentence batches + re-run) can always complete.
#[derive(Debug)]
pub enum BatchJob {
    /// Per-sentence batch pass (at the sentence's EOS).
    Sentence {
        paragraph_id: ParagraphId,
        sentence_id: SentenceId,
        pcm: Arc<Vec<i16>>,
        sr: u32,
    },
    /// Whole-paragraph re-run over the concatenated PCM (at settle; multi-sentence paragraphs
    /// only — single-sentence ones reuse the sentence-level job).
    Paragraph {
        paragraph_id: ParagraphId,
        pcm: Arc<Vec<i16>>,
        sr: u32,
    },
}

/// The outcome of one [`BatchJob`] — exactly one per job. `text: None` when `recognize`
/// failed (remote network) or returned empty; consumers fall back to streaming via
/// [`crate::VadSentence::best_text`]. `asr_ms` = the `recognize` wall-clock (perf metric).
#[derive(Debug)]
pub enum BatchJobResult {
    Sentence {
        paragraph_id: ParagraphId,
        sentence_id: SentenceId,
        text: Option<String>,
        asr_ms: u64,
    },
    Paragraph {
        paragraph_id: ParagraphId,
        text: Option<String>,
        asr_ms: u64,
    },
}

use anyhow::{bail, Result};
use tracing::{debug, info, warn};

use crate::audio_store::{AudioStore, DEFAULT_CAP_SAMPLES};
use crate::buffer::AudioRing;
use crate::scout::ScoutAudioSource;
use crate::{AudioId, SentenceId, Stage1Event, VadEventKind, VadSentence, VadParagraph, ParagraphId};
// ONNX 语音栈在 dp-models(feature `speech`)——audio-aura 不再直接依赖 sherpa-onnx。
use dp_models::onnx::{
    AsrBackend, AsrConfig, OnnxRuntimeManager, StreamingAsrConfig, StreamingSession, VadConfig,
    WINDOW,
};
use dp_models::{http::HttpAsr, AsrProvider, ProviderKind};

/// Default ring capacity: 10 min @ 16 kHz mono (~19 MB).
const DEFAULT_RING_CAP: usize = 16_000 * 600;
/// Streaming-partial decode cadence: every N paragraphs (~0.5s @ 32ms Silero paragraphs).
const PARTIAL_EVERY_FRAMES: u32 = 15;
/// Stale-session watchdog: reset the streaming session when its partial has been UNCHANGED
/// this long AND no EOS came — that means VAD never latched (audio below `threshold` =
/// discard-by-design), and its residue (hallucinated repetitions included) must NOT leak
/// into whatever sentence closes next (2026-08-17 实测:35s 悬置会话把上一句幻觉文本卷进
/// 下一句). Real speech never trips this: a ≥min_silence pause closes the sentence via EOS,
/// which resets the session long before the partial could go stale.
const STALE_SESSION_RESET: Duration = Duration::from_secs(8);

/// VAD 门控流式的 lead-in 帧数(每帧 32ms):detected() 翻转起音时补喂最近 ~0.5s 的帧,
/// 让 soft onset 进入流式/batch(Silero 要几帧过阈值,detected 翻转晚于真实起音)。
const LEAD_IN_FRAMES: usize = 16;
// batch 实时性约束(实时管线,不能死等):
// - 每请求**硬超时** + **断链熔断**都在 `dp_models::http::HttpAsr` 内
//   (ASR_TIMEOUT=3s,断链即窗口内不发送)—— 这里**单发不重试**:失败/超时/熔断 →
//   立即 `None` → 整流 + 就绪定稿照常往下,绝不为了 batch 卡住定稿。
// - 重试会成倍放大最坏等待(2×超时),与"实时"矛盾;断链由熔断快速跳过 + 窗口后自愈,
//   不需要在 worker 里重试。

/// Resolve a `MODELS::<sub-path>` model entry. A custom `models_dir` (config override) wins —
/// the sub-path is joined onto it; otherwise the shared `MODELS` namespace resolves via
/// FileLoader (dev: workspace `assets/models/`, prod: `~/.desk-pilot/models/`).
fn resolve_model(models_dir: Option<&str>, rel: &str) -> String {
    let sub = rel.strip_prefix("MODELS::").unwrap_or(rel);
    match models_dir {
        Some(dir) => format!("{dir}/{sub}"),
        None => shared::loader!()
            .resolve(rel)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Config for [`OnnxStage1Recognizer`] — paths + params for the VAD, batch ASR, and streaming ASR,
/// plus the omni-scout address, ring capacity, and the connection `active` flag.
#[derive(Clone)]
pub struct Stage1Config {
    pub scout_addr: String,
    /// Custom model-root override (config `asr.local.model_dir` / `llm.model_dir`): all
    /// `MODELS::` paths resolve under it instead of the shared namespace. `None` = namespace.
    pub models_dir: Option<String>,
    pub vad: VadConfig,
    pub asr: AsrConfig,
    pub streaming: StreamingAsrConfig,
    pub ring_cap_samples: usize,
    /// 客户端请求 scout 的推流 cadence(ms):`?chunk_ms=N` 让 scout 把源 buffer 聚合成
    /// N-ms 的 HTTP chunk 再推(不能快过 scout 的 node.quantum)。None = 不传参,scout
    /// 按自身 quantum 速率推。纯网络层优化——消费循环照样重切成 32ms 窗喂 VAD。
    pub scout_chunk_ms: Option<u64>,
    /// Batch ASR backend: `Local` (lib sherpa OnnxAsr) or `Remote` (HTTP, OpenAI-compatible).
    /// Streaming ASR + VAD stay local sherpa regardless (real-time partials need low latency).
    pub asr_kind: ProviderKind,
    /// ★Merge-paragraph gap (seconds) — the UPPER bound of the medium-interval paragraph. VAD fires
    /// EOS on every pause ≥ `min_silence` (kept low, ~1.0s, so each sentence's batch pass kicks
    /// in fast); a following sentence joins the SAME paragraph when the inter-speech silence <
    /// this. Only a gap ≥ this (or no new speech for this long) closes the paragraph →
    /// `ParagraphEdge`. The lower bound is implicit: `min_silence` is what splits sentences in the
    /// first place, so the effective paragraph is (min_silence, merge_gap) ≈ 1–2.5s. Decouples
    /// "VAD sensitivity" from "what's one utterance". 0 → every sentence is its own paragraph.
    pub merge_gap_s: f64,
    /// Batch-ASR switch (config `asr.backend: disable`): false → the batch model is NOT
    /// loaded and every batch pass returns empty (`batch_text` stays `None` — the legal
    /// "batch unavailable" state; consumers fall back to streaming text by design).
    /// Streaming + VAD unaffected. Defaults to true.
    pub batch_enabled: bool,
    /// batch job 自管开关(round12 异步化):`false` → 消费循环**不投** `BatchJob`
    /// (句 EOS / 段 settle 都只发事件)—— batch pass 由 Pipeline 的 per-paragraph
    /// 异步任务经 [`Self::recognize_once`] 自建。默认 `true`(投递给 batch worker,
    /// 旧编排);`Pipeline::assemble` 置 `false`。
    pub batch_jobs: bool,
    /// Shared connection toggle (see [`ScoutAudioSource::with_active`]). Flip to false to stop
    /// ingesting from scout (does NOT kill scout). Defaults to true.
    pub active: Arc<AtomicBool>,
    /// 运行信号(idle 深度睡眠):false → `run` 退出消费循环, 断开 scout; daemon 在下一个客户端
    /// 连接时置回 true 唤醒。与 `active`(scout 开关, 用户可单独控制) 独立。默认 true。
    pub running: Arc<AtomicBool>,
    /// 主动归档信号(IME 侧"我说完了"—— 分字符键 `'` 触发):run 循环见 true 且存在可归档
    /// 段落 → 跳过 `merge_gap` 剩余等待,立即整段 batch(`ParagraphEdge`)。消费中/说话中保持
    /// 挂起(EOS 未到,立即切窗会截断);无段落时消费掉(空按)。默认 false。
    pub flush_paragraph: Arc<AtomicBool>,
}

impl Stage1Config {
    /// Sensible defaults — model paths resolved via `shared` namespace `MODELS` (declared in
    /// this crate's `Cargo.toml` `[package.metadata.shared]`). Dev: `<workspace>/assets/models/`;
    /// prod: `~/.audio-aura/models/`. No `base` param needed — the caller never sees paths.
    pub fn new(scout_addr: impl Into<String>) -> Self {
        Self::with_models_dir(scout_addr, None)
    }

    /// [`Self::new`] with a custom model root: every `MODELS::` path (VAD / streaming / batch
    /// ASR) resolves under `models_dir` instead of the shared namespace — config 钮
    /// `asr.local.model_dir`. Builders resolve through the same root.
    pub fn with_models_dir(scout_addr: impl Into<String>, models_dir: Option<String>) -> Self {
        // TODO: 在一个 new 函数中使用了 IO 操作，会失败，将 IO 拆出去作为另一个函数
        let dir = models_dir.clone();
        let p = |rel: &str| -> String { resolve_model(dir.as_deref(), rel) };
        Self {
            scout_addr: scout_addr.into(),
            models_dir,
            vad: VadConfig {
                model: p("MODELS::silero-vad/silero_vad.onnx"),
                ..Default::default()
            },
            asr: AsrConfig {
                backend: AsrBackend::SenseVoice {
                    model: p("MODELS::sensevoice/model.int8.onnx"),
                    language: "auto".into(),
                },
                tokens: p("MODELS::sensevoice/tokens.txt"),
                ..Default::default()
            },
            streaming: StreamingAsrConfig {
                encoder: p("MODELS::zipformer-streaming-zh-en/encoder-epoch-99-avg-1.onnx"),
                decoder: p("MODELS::zipformer-streaming-zh-en/decoder-epoch-99-avg-1.onnx"),
                joiner: p("MODELS::zipformer-streaming-zh-en/joiner-epoch-99-avg-1.onnx"),
                tokens: p("MODELS::zipformer-streaming-zh-en/tokens.txt"),
                bpe_vocab: p("MODELS::zipformer-streaming-zh-en/bpe.vocab"),
                ..Default::default()
            },
            ring_cap_samples: DEFAULT_RING_CAP,
            scout_chunk_ms: None,
            asr_kind: ProviderKind::Local,
            merge_gap_s: 5.0,
            batch_enabled: true,
            batch_jobs: true,
            active: Arc::new(AtomicBool::new(true)),
            running: Arc::new(AtomicBool::new(true)),
            flush_paragraph: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Streaming engine selection (config `asr.stream.model`; streaming is ALWAYS local):
    /// - "zipformer" — the default, 2023 bilingual zh-en (tens-of-thousands-hours training);
    /// - "x-asr" — 2026, ~0.16B zipformer transducer trained on ~1M hours zh-en
    ///   code-switch (repo: Gilgamesh-J/X-ASR; official chunk-480ms fp32 export, outputs
    ///   PUNCTUATED text). Beats SenseVoice-small on published benchmarks despite 10×
    ///   fewer params than Qwen3-ASR. 160/960/1920ms chunk variants exist in the repo.
    pub fn with_stream_engine(mut self, engine: &str) -> Result<Self> {
        match engine {
            "zipformer" => Ok(self), // the default paths from with_models_dir
            "x-asr" => {
                let dir = self.models_dir.clone();
                let p = |rel: &str| resolve_model(dir.as_deref(), rel);
                self.streaming = StreamingAsrConfig {
                    encoder: p("MODELS::x-asr/encoder-480ms.onnx"),
                    decoder: p("MODELS::x-asr/decoder-480ms.onnx"),
                    joiner: p("MODELS::x-asr/joiner-480ms.onnx"),
                    // MUST be the official two-column "token id" format — sherpa builds its
                    // token→id map from the index column (a single-column rewrite breaks it).
                    tokens: p("MODELS::x-asr/tokens.txt"),
                    // Exported from lang_5000/bpe.model via sentencepiece ("piece score"
                    // lines) — sherpa needs it to tokenize raw-text hotwords (cjkchar+bpe).
                    bpe_vocab: p("MODELS::x-asr/bpe.vocab"),
                    ..Default::default()
                };
                Ok(self)
            }
            other => bail!(
                "unsupported streaming engine {other:?} (supported: \"zipformer\" | \"x-asr\")"
            ),
        }
    }

    /// Use a remote HTTP ASR (OpenAI-compatible `/v1/audio/transcriptions`) instead of local
    /// sherpa. `model` = 服务端模型名(必传;OpenAI 规范要求 multipart 带 `model` 字段,
    /// 与目标服务如 dp-router.yaml `models[].name` 对齐)。
    /// 流式 ASR + VAD 仍本地 sherpa(实时 partial 需要低延迟)。
    pub fn with_remote_asr(mut self, endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        self.asr_kind = ProviderKind::Remote {
            endpoint: endpoint.into(),
            model: model.into(),
        };
        self
    }
}

/// A Stage1 recognizer: audio in → [`Stage1Event`]s out. `run` blocks forever (drives the
/// ingest+consume loop) and invokes `on_event` for each interim partial / settled sentence /
/// closed paragraph.
pub trait Stage1Recognizer {
    /// 跑消费循环直到 `running` 被置 false(idle 深度睡眠)→ 返回。daemon 恢复时重新调用。
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) -> ();
}

/// Batch ASR turned off (`asr.backend: disable`): every pass yields empty text, which the
/// executor maps to `batch_text: None` — the legal "batch unavailable" state consumers
/// already handle by falling back to streaming text.
struct DisabledAsr;

impl AsrProvider for DisabledAsr {
    fn recognize(&self, _pcm: &[i16], _sample_rate: u32) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

/// ONNX-backed Stage1 recognizer (Silero VAD + streaming Zipformer + batch ASR via the single
/// [`OnnxRuntimeManager`]). **Spawns no threads** — the three blocking jobs (ingest / consume
/// loop / batch worker) are exposed as methods the `Pipeline` runs on threads it owns. The ring
/// is shared with the ingest thread; the consume loop runs on its caller's thread; batch jobs
/// are handed to the worker via an mpsc channel (see [`Self::new`]).
pub struct OnnxStage1Recognizer {
    mgr: Arc<OnnxRuntimeManager>,
    /// Batch ASR as a trait object: local OnnxAsr (from `mgr`) or remote HttpAsr. Streaming/VAD
    /// stay in `mgr` (always local sherpa).
    batch_asr: Arc<dyn AsrProvider>,
    ring: Arc<Mutex<AudioRing>>,
    /// Wakes the consume loop when the ingest thread pushes frames (no polling).
    ring_cv: Arc<Condvar>,
    /// Merge-paragraph gap (s) — see [`Stage1Config::merge_gap_s`].
    merge_gap_s: f64,
    active: Arc<AtomicBool>,
    /// idle 运行信号:false → run 退出循环(深度睡眠)。
    running: Arc<AtomicBool>,
    /// 主动归档信号(`Stage1Config::flush_paragraph`)—— run 循环消费,见下。
    flush_paragraph: Arc<AtomicBool>,
    /// The PCM store: sentences' clips live here by id (shared `Arc`) until their paragraph
    /// settles.
    audio_store: Arc<AudioStore>,
    /// scout 地址 — [`Self::run_ingest`] 用它建连接(ingest 线程由 Pipeline 创建)。
    scout_addr: String,
    /// 客户端请求 scout 的推流 cadence(ms)(`run_ingest` 用)。
    scout_chunk_ms: Option<u64>,
    /// 句级/段级 batch job 通道 sender — 消费循环 EOS/settle 时入队;receiver 由
    /// [`Self::new`] 交给 `Pipeline`,后者 spawn batch worker 线程跑 [`Self::run_batch_worker`]。
    /// `batch_jobs = false`(round12:Pipeline 异步任务自管 batch)时**不投递**。
    batch_tx: mpsc::Sender<BatchJob>,
    /// [`Stage1Config::batch_jobs`] 快照 —— 消费循环 enqueue guard。
    batch_jobs: bool,
}

impl OnnxStage1Recognizer {
    /// Build models from `cfg` and warm them. **Spawns no threads** (this crate never does —
    /// the `Pipeline` owns all of them). Returns the recognizer + the RECV end of the batch-job
    /// channel: the consume loop sends [`BatchJob`]s (at EOS / settle) and the `Pipeline` hands
    /// `batch_rx` to the worker thread it spawns for [`Self::run_batch_worker`].
    pub fn new(cfg: Stage1Config) -> Result<(Self, mpsc::Receiver<BatchJob>)> {
        // Batch ASR: Local → OnnxAsr lives in the mgr; Remote → HttpAsr (mgr skips .asr());
        // batch disabled → no batch model loaded at all, DisabledAsr stands in (empty result
        // ⇒ batch_text: None, the legal fallback state).
        let mgr = match (&cfg.asr_kind, cfg.batch_enabled) {
            (ProviderKind::Local, true) => Arc::new(
                OnnxRuntimeManager::builder()
                    .vad(cfg.vad.clone())
                    .asr(cfg.asr.clone())
                    .streaming_asr(cfg.streaming.clone())
                    .build()?,
            ),
            (ProviderKind::Local, false) | (ProviderKind::Remote { .. }, _) => Arc::new(
                OnnxRuntimeManager::builder()
                    .vad(cfg.vad.clone())
                    .streaming_asr(cfg.streaming.clone())
                    .build()?, // no local batch ASR — remote HttpAsr or batch-off
            ),
        };
        let batch_asr: Arc<dyn AsrProvider> = match (&cfg.asr_kind, cfg.batch_enabled) {
            (_, false) => Arc::new(DisabledAsr),
            (ProviderKind::Local, _) => {
                Arc::clone(mgr.asr().expect("local asr just loaded")) as Arc<dyn AsrProvider>
            }
            (ProviderKind::Remote { endpoint, model }, _) => {
                Arc::new(HttpAsr::new(endpoint.clone(), model.clone()))
            }
        };
        mgr.warm();
        let ring = Arc::new(Mutex::new(AudioRing::new(cfg.ring_cap_samples)));
        let ring_cv = Arc::new(Condvar::new());
        let (batch_tx, batch_rx) = mpsc::channel();
        Ok((
            Self {
                mgr,
                ring,
                ring_cv,
                merge_gap_s: cfg.merge_gap_s,
                active: cfg.active,
                running: cfg.running,
                flush_paragraph: cfg.flush_paragraph,
                audio_store: Arc::new(AudioStore::new(DEFAULT_CAP_SAMPLES)),
                batch_asr,
                scout_addr: cfg.scout_addr.clone(),
                scout_chunk_ms: cfg.scout_chunk_ms,
                batch_tx,
                batch_jobs: cfg.batch_jobs,
            },
            batch_rx,
        ))
    }

    /// Use an already-loaded [`OnnxRuntimeManager`] (e.g. shared with another stage). Same
    /// no-thread contract as [`Self::new`]: returns `(Self, batch_rx)`.
    pub fn new_with_mgr(mgr: Arc<OnnxRuntimeManager>, cfg: Stage1Config) -> Result<(Self, mpsc::Receiver<BatchJob>)> {
        let batch_asr: Arc<dyn AsrProvider> = match (&cfg.asr_kind, cfg.batch_enabled) {
            (_, false) => Arc::new(DisabledAsr),
            (ProviderKind::Local, _) => {
                Arc::clone(mgr.asr().expect("local mgr must carry the batch ASR")) as Arc<dyn AsrProvider>
            }
            (ProviderKind::Remote { endpoint, model }, _) => {
                Arc::new(HttpAsr::new(endpoint.clone(), model.clone()))
            }
        };
        let ring = Arc::new(Mutex::new(AudioRing::new(cfg.ring_cap_samples)));
        let ring_cv = Arc::new(Condvar::new());
        let (batch_tx, batch_rx) = mpsc::channel();
        Ok((
            Self {
                mgr,
                ring,
                ring_cv,
                merge_gap_s: cfg.merge_gap_s,
                active: cfg.active,
                running: cfg.running,
                flush_paragraph: cfg.flush_paragraph,
                audio_store: Arc::new(AudioStore::new(DEFAULT_CAP_SAMPLES)),
                batch_asr,
                scout_addr: cfg.scout_addr.clone(),
                scout_chunk_ms: cfg.scout_chunk_ms,
                batch_tx,
                batch_jobs: cfg.batch_jobs,
            },
            batch_rx,
        ))
    }

    /// Access the underlying ONNX model manager (e.g. for diagnostics / direct ASR calls).
    pub fn manager(&self) -> &Arc<OnnxRuntimeManager> {
        &self.mgr
    }

    /// The PCM store this recognizer owns — clips are addressable by [`AudioId`] until their
    /// paragraph settles (then evicted; the paragraph's `Arc<Vec<i16>>` is the surviving copy).
    pub fn audio_store(&self) -> &Arc<AudioStore> {
        &self.audio_store
    }

    /// Blocking ingest loop: omni-scout `/audio` (TCP) → [`AudioRing`] + condvar notify, with
    /// auto-reconnect (2s backoff). Runs FOREVER (reconnects; `active=false` pauses the
    /// connection). **Blocking — the `Pipeline` runs it on its own thread** (this crate spawns
    /// none).
    pub fn run_ingest(&self) -> ! {
        let src = ScoutAudioSource::with_active(self.scout_addr.clone(), WINDOW, Arc::clone(&self.active))
            .with_chunk_ms(self.scout_chunk_ms);
        src.stream(
            move |win| {
                let mut g = self.ring.lock().unwrap();
                g.push(win);
                drop(g);
                // Wake the consume loop — it sleeps on the condvar between frames
                // (deadline-driven, no polling).
                self.ring_cv.notify_all();
            },
            Duration::from_secs(2),
        )
    }

    /// Run the blocking batch ASR **once** (no retry — real-time: a hang/timeout/circuit-trip
    /// must NOT stall finalization). `Ok(non-empty)` → `Some`; `Ok(empty)` (noise/silence) and
    /// `Err` (timeout / 断链 / 熔断) → `None` (caller falls back to streaming). The timeout +
    /// 断链熔断 live in [`dp_models::http::HttpAsr`] (ASR_TIMEOUT=3s, 断链即窗口内不发送);
    /// this just surfaces the outcome + logs the failure reason so "丢 batch" is diagnosable.
    pub fn recognize_once(&self, pcm: &[i16], sr: u32, what: &str, paragraph_id: ParagraphId) -> Option<String> {
        match self.batch_asr.recognize(pcm, sr) {
            Ok(text) if !text.trim().is_empty() => Some(text),
            Ok(_) => {
                debug!(what, paragraph_id, "batch 识别成功但文本为空(噪声/静音)→ 回退流式");
                None
            }
            Err(e) => {
                // 失败/超时/熔断 —— 立即回退流式,不等、不重试。这是"丢 BatchSentence"的
                // 唯一来源;日志区分原因(超时 3s / 断链 / 熔断窗口)。
                warn!(
                    error = %e,
                    what,
                    paragraph_id,
                    "batch 失败(超时/断链/熔断)→ 回退流式,立即继续(实时,不等)"
                );
                None
            }
        }
    }

    /// Blocking batch worker: drain `rx` and run each job's batch ASR (the blocking
    /// `AsrProvider::recognize`, **once per job** — see [`Self::recognize_once`]), calling
    /// `on_result` **exactly once per job** — failure/empty map to `text: None`, jobs are never
    /// dropped (the pipeline's readiness gate relies on every job producing one result).
    /// **Blocking — the `Pipeline` runs it on its own thread.**
    pub fn run_batch_worker(&self, rx: mpsc::Receiver<BatchJob>, on_result: &mut dyn FnMut(BatchJobResult)) {
        for job in rx {
            match job {
                BatchJob::Sentence {
                    paragraph_id,
                    sentence_id,
                    pcm,
                    sr,
                } => {
                    let t0 = Instant::now();
                    let text = self.recognize_once(&pcm, sr, "句级", paragraph_id);
                    let asr_ms = t0.elapsed().as_millis() as u64;
                    debug!(
                        paragraph_id,
                        sentence_id,
                        asr_ms,
                        batch = text.as_deref().unwrap_or("(none)"),
                        "句级 batch 完成(异步 worker)"
                    );
                    on_result(BatchJobResult::Sentence {
                        paragraph_id,
                        sentence_id,
                        text,
                        asr_ms,
                    });
                }
                BatchJob::Paragraph { paragraph_id, pcm, sr } => {
                    let t0 = Instant::now();
                    let text = self.recognize_once(&pcm, sr, "段落级重跑", paragraph_id);
                    let asr_ms = t0.elapsed().as_millis() as u64;
                    debug!(
                        paragraph_id,
                        asr_ms,
                        batch = text.as_deref().unwrap_or("(none)"),
                        "段落级 batch 重跑完成(异步 worker)"
                    );
                    on_result(BatchJobResult::Paragraph { paragraph_id, text, asr_ms });
                }
            }
        }
    }
}

/// Block until a full Silero paragraph is available in the ring (wakes on the ingest thread's
/// condvar notify). `timeout: Some` additionally caps the wait — `None` return means the
/// deadline fired (the caller re-runs its time-based checks); `timeout: None` parks until
/// audio arrives (no timer at all — nothing time-based is pending).
fn wait_frame(
    ring: &Mutex<AudioRing>,
    ring_cv: &Condvar,
    frame_samples: usize,
    timeout: Option<Duration>,
) -> Option<Vec<i16>> {
    let mut g = ring.lock().unwrap();
    if g.has_frame(frame_samples) {
        return Some(g.drain(frame_samples));
    }
    let mut g = match timeout {
        Some(t) => {
            let (g, _timed_out) =
                ring_cv.wait_timeout_while(g, t, |r| !r.has_frame(frame_samples)).unwrap();
            g
        }
        None => ring_cv.wait_while(g, |r| !r.has_frame(frame_samples)).unwrap(),
    };
    if g.has_frame(frame_samples) {
        Some(g.drain(frame_samples))
    } else {
        None
    }
}

// ── Paragraph tracker: pure paragraphing decisions over wall-clock SOS/EOS (unit-testable, no I/O) ──
// The recognizer owns the ASR side (sessions, batch passes, the AudioStore); this tracker owns
// ONLY the boundary math — which sentence belongs to which paragraph, and when a paragraph closes.

/// The open paragraph: its settled sentences + whether a sentence is in progress (SOS seen,
/// EOS pending). The in-progress sentence's id/timing live recognizer-side ([`ActiveSession`]);
/// the tracker only needs "is one active" for settle suppression.
struct OpenParagraph {
    paragraph_id: ParagraphId,
    sentences: Vec<VadSentence>,
    active: bool,
}

/// A paragraph closed by a big gap or the settle-timeout — the recognizer turns this into a
/// [`VadParagraph`] (concat PCM + paragraph-level batch re-run) and emits `ParagraphEdge`.
struct SettledParagraph {
    paragraph_id: ParagraphId,
    sentences: Vec<VadSentence>,
}

struct ParagraphTracker {
    merge_gap_s: f64,
    next_sentence_id: SentenceId,
    /// 最近分配的 paragraph id(供 `prospective` 给未开段落预生成;下一个随机 id)。
    last_win_id: ParagraphId,
    open: Option<OpenParagraph>,
}

impl ParagraphTracker {
    fn new(merge_gap_s: f64) -> Self {
        Self { merge_gap_s, next_sentence_id: 1, last_win_id: 0, open: None }
    }

    /// 生成一个**随机** paragraph id(基于系统时间亚微秒纳秒,无依赖、不可预测,
    /// `u64` 足够宽不会快速碰撞)。用随机而非递增 —— 避免可预测性,也让重连后
    /// 历史段落 id 与新段落不产生"连续/相邻"的假关联。
    fn next_random_win_id(&mut self) -> ParagraphId {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        // 混入自增计数:同一纳秒内连续两次也会不同(仅作防碰撞,不是"递增 id")。
        self.last_win_id += 1;
        let mut id = nanos ^ (self.last_win_id.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        if id == 0 {
            id = 1;
        }
        self.last_win_id = id;
        id
    }

    /// VAD StartOfSpeech. NOTE: the SOS is RETROACTIVE — it fires at the sentence's EOS instant
    /// (its wall-clock IS the EOS time, NOT the speech onset), so the merge/split decision
    /// CANNOT happen here (using the EOS instant as the onset would inflate every gap by the
    /// sentence's own duration and settle on EVERY sentence — the "paragraph never has >1 sentence"
    /// bug). This only allocates the sentence id + marks the paragraph active; the settle decision
    /// moves to [`Self::on_eos`], which back-derives the true speech onset from the PCM.
    fn on_sos(&mut self) -> SentenceId {
        if self.open.is_none() {
            let id = self.next_random_win_id();
            self.open = Some(OpenParagraph { paragraph_id: id, sentences: Vec::new(), active: false });
        }
        let sentence_id = self.next_sentence_id;
        self.next_sentence_id += 1;
        self.open.as_mut().expect("paragraph just ensured").active = true;
        sentence_id
    }

    /// Settle the open paragraph iff the gap from `onset` (the NEXT sentence's true speech start)
    /// back to its last sentence ≥ merge_gap. `onset` must be the back-derived start, not the
    /// retroactive SOS instant.
    fn settle_if_gap(&mut self, onset: f64) -> Option<SettledParagraph> {
        let gap = {
            let w = self.open.as_ref()?;
            let last = w.sentences.last()?;
            onset - last.end_s
        };
        if gap >= self.merge_gap_s {
            self.take_open()
        } else {
            None
        }
    }

    /// Record a completed sentence. Settles the open paragraph FIRST when the gap since its last
    /// sentence ≥ merge_gap (using `sentence.start_s`, the BACK-DERIVED true onset), then pushes this
    /// sentence into the (possibly fresh) paragraph. Returns (settled spans, paragraph id, ALL sentences
    /// so far) — the payload IS the paragraph, so Stage2 stays stateless.
    fn on_eos(&mut self, sentence: VadSentence) -> (Option<SettledParagraph>, ParagraphId, Vec<VadSentence>) {
        let settled = self.settle_if_gap(sentence.start_s);
        if self.open.is_none() {
            // First sentence, or the previous paragraph just settled.
            let id = self.next_random_win_id();
            self.open = Some(OpenParagraph { paragraph_id: id, sentences: Vec::new(), active: false });
        }
        let w = self.open.as_mut().expect("paragraph just ensured");
        w.active = false;
        w.sentences.push(sentence);
        (settled, w.paragraph_id, w.sentences.clone())
    }

    /// Settle-timeout probe (call every loop tick with the current wall-clock). Closes the
    /// paragraph when it has been silent (no active speech) for ≥ `merge_gap_s` — this is how the
    /// TRAILING paragraph finalizes. Suppressed while a sentence is in progress AND while `speaking`
    /// is true — the streaming session still has a non-empty partial, i.e. someone is talking
    /// right now but this VAD's SOS for that speech hasn't arrived yet (it's RETROACTIVE, comes
    /// with EOS). Without this suppression the wall-clock timeout would fire mid-sentence and
    /// split the next sentence into a fresh paragraph — the "paragraph never has >1 sentence" bug.
    fn check_settle(&mut self, now: f64, speaking: bool) -> Option<SettledParagraph> {
        let w = self.open.as_ref()?;
        if w.active || speaking {
            return None;
        }
        let last = w.sentences.last()?;
        if now - last.end_s >= self.merge_gap_s {
            self.take_open()
        } else {
            None
        }
    }

    /// 主动归档(用户侧"我说完了"信号):跳过 `merge_gap` 剩余等待,立即关闭开放段落。
    /// 语义与 [`Self::check_settle`] 的 suppress 条件一致 —— 有句进行中(`active`)或
    /// 段落为空时不动(调用方负责保持 flush 挂起重试);`speaking` 的墙钟抑制由调用方
    /// 判断(它不在 tracker 状态里)。
    fn force_settle(&mut self) -> Option<SettledParagraph> {
        let w = self.open.as_ref()?;
        if w.active || w.sentences.is_empty() {
            return None;
        }
        self.take_open()
    }

    /// 是否有开放段落(含进行中段)—— flush 挂起与否的判据:段落在 → 保持挂起等 EOS;
    /// 无段落 → flush 落空,消费掉标记。
    fn has_open_paragraph(&self) -> bool {
        self.open.is_some()
    }

    /// Seconds until [`Self::check_settle`] would close the open paragraph (None = no pending
    /// settle: nothing open, no sentences yet, a sentence in progress, or `speaking` — the next
    /// sentence's speech is ongoing but its SOS hasn't arrived yet). Drives the consume loop's
    /// condvar deadline — wake exactly when the trailing paragraph is due, not on a poll cadence.
    fn settle_deadline(&self, now: f64, speaking: bool) -> Option<f64> {
        let w = self.open.as_ref()?;
        if w.active || speaking {
            return None;
        }
        let last = w.sentences.last()?;
        Some((self.merge_gap_s - (now - last.end_s)).max(0.0))
    }

    fn take_open(&mut self) -> Option<SettledParagraph> {
        self.open.take().map(|w| SettledParagraph { paragraph_id: w.paragraph_id, sentences: w.sentences })
    }

    /// The ids the sentence currently being spoken WILL get: the open paragraph's id (or the next
    /// one when nothing is open) + the next sentence id. Used to key live `StreamFragment`
    /// partials —
    /// this VAD emits SOS RETROACTIVELY (with EOS), so the real assignment only exists at EOS.
    /// Authoritative grouping arrives with the `Batch`/`ParagraphEdge` events.
    ///
    /// paragraph id 是随机的;未开段落时给一个"预测"随机值(仅用于给 partial 预分组,
    /// 实际分配在 EOS 用 [`next_random_win_id`](Self::next_random_win_id))。
    fn prospective(&self) -> (ParagraphId, SentenceId) {
        let w = self
            .open
            .as_ref()
            .map(|w| w.paragraph_id)
            .unwrap_or_else(|| self.last_win_id.wrapping_add(1).max(1));
        (w, self.next_sentence_id)
    }
}

/// Turn settled spans into a [`VadParagraph`] and emit `ParagraphEdge`: concat the clips from
/// the store (once — the paragraph keeps the shared `Arc`), **enqueue the paragraph re-run as a
/// batch job (async — the consume loop is never blocked by it)**, then evict the clips. The
/// event carries `batch_text: None` (in-flight); the result arrives via
/// [`Stage1Event::ParagraphBatchReady`]. An all-discarded paragraph (no sentences) emits nothing
/// and just vanishes.
fn emit_paragraph_edge(
    settled: SettledParagraph,
    store: &AudioStore,
    batch_tx: &mpsc::Sender<BatchJob>,
    sr: u32,
    batch_jobs: bool,
    on_event: &mut dyn FnMut(Stage1Event),
) {
    if settled.sentences.is_empty() {
        return;
    }
    let ids: Vec<AudioId> = settled.sentences.iter().map(|s| s.audio_id).collect();
    let pcm = Arc::new(store.concat(&ids));
    let sentence_count = settled.sentences.len();
    let streaming_text =
        settled.sentences.iter().map(|s| s.streaming_text.as_str()).collect::<String>();
    let start_s = settled.sentences.first().map(|s| s.start_s).unwrap_or(0.0);
    let end_s = settled.sentences.last().map(|s| s.end_s).unwrap_or(0.0);
    // ★ 顺序不变式(竞态防护,同 finalize_sentence):先发 `ParagraphEdge` 事件(占位建
    // pending),再入队段级重跑 job —— 重跑结果(ParagraphBatchReady)必在事件之后落到同一条
    // stage2 FIFO 通道。
    on_event(Stage1Event::ParagraphEdge {
        paragraph: VadParagraph {
            id: settled.paragraph_id,
            sentences: settled.sentences,
            start_s,
            end_s,
            streaming_text,
            // ASYNC re-run: None on this event; ParagraphBatchReady patches it (pipeline).
            batch_text: None,
            // Wall-clock measured by the batch worker; the pipeline fills it in on
            // ParagraphBatchReady (single-sentence paragraphs never re-run → stays 0).
            batch_asr_ms: 0,
            pcm: Arc::clone(&pcm),
        },
        sr,
    });
    // ★单句段落免重跑:段落 batch 的意义是"跨句上下文重新整听"——只有一句时拼接 PCM 与该句
    // PCM 完全相同,句级 batch job 已覆盖,不再投递重跑 job(省掉大多数段落的一整次 batch
    // 调用)。单句段落的定稿文本由 pipeline 在句级 SentenceBatchReady 到达时就绪(见
    // pipeline.rs Finalizer:单句段落 para_done 在 ParagraphEdge 即置位)。
    // round12:`batch_jobs = false`(Pipeline 异步任务自管 batch)→ 不投递,段落重跑由
    // pipeline 段任务经 recognize_once(paragraph.pcm) 自建。
    if sentence_count == 1 {
        debug!(paragraph_id = settled.paragraph_id, "单句段落——复用句级 batch,跳过整段重跑(不投递 job)");
    } else if !batch_jobs {
        debug!(paragraph_id = settled.paragraph_id, "batch_jobs=false — 段落重跑由 pipeline 段任务自建(不投递 job)");
    } else if let Err(e) = batch_tx.send(BatchJob::Paragraph {
        paragraph_id: settled.paragraph_id,
        pcm,
        sr,
    }) {
        warn!(error = %e, paragraph_id = settled.paragraph_id, "batch worker gone — paragraph re-run job dropped");
    }
    // The paragraph's Arc PCM is now the only remaining copy — release the per-sentence clips
    // (the re-run job shares the paragraph's Arc, so eviction is safe).
    store.evict(&ids);
}

/// The live streaming session + its partial-throttle state. D1 adaptation: sherpa's VAD
/// emits SOS RETROACTIVELY (together with EOS — the sentence only pops complete), so the
/// session CANNOT be created at speech onset. Instead it is fed CONTINUOUSLY and RESET at
/// every sentence boundary (EOS) and paragraph settle — each session therefore covers exactly
/// [previous boundary, this EOS] ≈ this one sentence (+ surrounding silence, which decodes
/// to nothing). Per-sentence attribution is preserved; live partials keep flowing.
struct ActiveSession {
    stream: StreamingSession,
    frames_since_partial: u32,
    last_partial: String,
    /// When `last_partial` last CHANGED (decayed text ⇒ stale ⇒ watchdog reset).
    last_change: Instant,
    /// Diagnostic: frames fed since the last reset.
    fed: u32,
    /// Every fed frame, accumulated — the EXACT audio this streaming session heard. At EOS this
    /// becomes the sentence's PCM (shared with the batch ASR), so streaming and batch see the
    /// same audio — including the soft onset BEFORE VAD's threshold crossing, which the VAD's
    /// own sentence cuts off (the "batch drops the first 2-3 chars" bug). Bounded by the sentence
    /// length (+ boundary silence), reset at every EOS / paragraph settle.
    pcm: Vec<i16>,
}

impl ActiveSession {
    fn new(stream: StreamingSession) -> Self {
        Self {
            stream,
            frames_since_partial: 0,
            last_partial: String::new(),
            last_change: Instant::now(),
            fed: 0,
            pcm: Vec::new(),
        }
    }
}

/// 取帧结果:拿到一帧去处理,或 park 后重跑循环(截止/节流触发)。
enum FrameResult {
    Frame(Vec<i16>),
    Parked,
}

impl OnnxStage1Recognizer {
    /// 取一帧(32ms)处理,或 park 后重跑循环。ring 有帧直接取;空则等音频/截止,
    /// 断流>2s 且有 partial 时喂合成静音逼 VAD EOS(100ms 节流,避免 CPU 空转)。
    fn drain_frame(
        &self,
        ring_empty_since: &mut Option<Instant>,
        sess: &Option<ActiveSession>,
        last_silence_feed: &mut Instant,
        wake_at: Option<Duration>,
    ) -> FrameResult {
        let mut g = self.ring.lock().unwrap();
        if g.has_frame(WINDOW) {
            *ring_empty_since = None;
            return FrameResult::Frame(g.drain(WINDOW));
        }
        drop(g);
        ring_empty_since.get_or_insert_with(Instant::now);
        let since = *ring_empty_since.as_ref().unwrap();
        let has_partial = sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
        if since.elapsed() > Duration::from_secs(2) && has_partial {
            // 断流:喂合成静音让 VAD 发 EOS(每 100ms 至多一帧,~1s 静音约 3s 墙钟)
            if last_silence_feed.elapsed() >= Duration::from_millis(100) {
                *last_silence_feed = Instant::now();
                debug!("ring empty > 2s during speech — feeding silence to force VAD EOS");
                FrameResult::Frame(vec![0i16; WINDOW])
            } else {
                match wait_frame(&self.ring, &self.ring_cv, WINDOW, Some(Duration::from_millis(100))) {
                    Some(f) => { *ring_empty_since = None; FrameResult::Frame(f) }
                    None => FrameResult::Parked,
                }
            }
        } else {
            // Park until the ingest pushes or the next deadline — 无轮询,空闲零唤醒.
            match wait_frame(&self.ring, &self.ring_cv, WINDOW, wake_at) {
                Some(f) => { *ring_empty_since = None; FrameResult::Frame(f) }
                None => FrameResult::Parked,
            }
        }
    }

    /// 喂流式会话(VAD 门控):`detected()` 为 true 时 accept+解码,起音翻转(false→true)补喂
    /// 最近 ~0.5s 的 lead-in(soft onset 进会话)。空闲 park,只累积有界 lead-in。
    /// `accept_waveform` 与 `pcm` 喂**完全相同**的帧 → 流式与 batch 共享同一句音频。
    fn feed_streaming(
        &self,
        sess: &mut Option<ActiveSession>,
        tracker: &mut ParagraphTracker,
        lead_in: &mut VecDeque<Vec<i16>>,
        speech_active: &mut bool,
        frame: &[i16],
        sr: u32,
        at_s: f64,
        v_detected: bool,
        on_event: &mut dyn FnMut(Stage1Event),
    ) {
        let (Some(asr), Some(a)) = (self.mgr.streaming_asr(), sess.as_mut()) else { return };
        if v_detected {
            if !*speech_active {
                // 起音:补喂 lead-in,让流式/batch 都听到 soft onset
                for chunk in lead_in.drain(..) {
                    a.stream.accept_waveform(sr as i32, &chunk);
                    a.pcm.extend_from_slice(&chunk);
                    a.fed += 1;
                }
                a.frames_since_partial = 0; // 补喂后重新起解码节拍
            }
            a.stream.accept_waveform(sr as i32, frame);
            a.pcm.extend_from_slice(frame); // 流式与 batch 共用同一句音频
            a.fed += 1;
            a.frames_since_partial += 1;
            if a.frames_since_partial >= PARTIAL_EVERY_FRAMES {
                let partial = asr.decode_and_result(&a.stream);
                if !partial.is_empty() && partial != a.last_partial {
                    let (paragraph_id, sentence_id) = tracker.prospective();
                    on_event(Stage1Event::StreamFragment {
                        paragraph_id,
                        sentence_id,
                        text: partial.clone(),
                        at_s,
                    });
                    a.last_partial = partial;
                    a.last_change = Instant::now();
                }
                a.frames_since_partial = 0;
            }
        } else {
            // 空闲:流式会话 park;只累积有界 lead-in(供下次起音补喂)
            lead_in.push_back(frame.to_vec());
            if lead_in.len() > LEAD_IN_FRAMES {
                lead_in.pop_front();
            }
        }
        *speech_active = v_detected;
    }

    /// 定稿一个 VAD 句(EOS 臂):finalize 流式会话 → streaming_text,句 PCM 入 store(共享
    /// `Arc`),**入队句级 batch job(异步——消费循环不阻塞)**,emit `Batch`(`batch_text: None`)
    /// 及可能的 `ParagraphEdge`。`fallback_pcm` = 流式未配置时的 VAD edge-extended 句。
    ///
    /// 噪声句不再在 EOS 丢弃:batch 异步后 EOS 时刻只有流式文本,若流式空就丢弃,会丢掉
    /// "流式没听出、batch 能听出"的真实语音(吞句的另一形态)。空句无文本贡献,由段落折叠
    /// 自然吸收;停滞幻觉由 8s 看门狗在下一句前清掉。
    fn finalize_sentence(
        &self,
        sess: Option<ActiveSession>,
        tracker: &mut ParagraphTracker,
        cur_sentence: &mut SentenceId,
        sr: u32,
        end_s: f64,
        fallback_pcm: Vec<i16>,
        fed: u32,
        on_event: &mut dyn FnMut(Stage1Event),
    ) {
        // 句 PCM = 流式 session 累积的完整音频(含句首 soft onset)——与流式听到的完全一致,
        // 区别只在 batch 一次整句听(大块)vs 流式逐帧听(小块)。流式未配置时 fallback VAD 句。
        // `Arc`:store / batch job 共享同一份分配,零拷贝。
        let sentence_pcm: Arc<Vec<i16>> =
            Arc::new(sess.as_ref().map(|a| a.pcm.clone()).unwrap_or(fallback_pcm));
        let streaming_text = match (self.mgr.streaming_asr(), sess.as_ref()) {
            (Some(asr), Some(a)) => asr.finalize_and_result(&a.stream),
            _ => String::new(),
        };
        // Speech onset back-derived from the PCM duration (SOS was retroactive, so its
        // wall-clock IS the EOS instant).
        let start_s = (end_s - sentence_pcm.len() as f64 / sr as f64).max(0.0);
        let sentence_id = *cur_sentence;
        let sentence = VadSentence {
            id: sentence_id,
            audio_id: self.audio_store.insert(Arc::clone(&sentence_pcm)),
            start_s,
            end_s,
            streaming_text: streaming_text.clone(),
            // ASYNC batch: the pass runs on the batch worker thread; the result arrives via
            // SentenceBatchReady. None here is the in-flight state (== the old "batch failed"
            // state for consumers — best_text falls back to streaming either way).
            batch_text: None,
        };
        let (settled, paragraph_id, sentences) = tracker.on_eos(sentence);
        // A big gap settled the previous paragraph FIRST — emit it before this sentence's Batch.
        if let Some(s) = settled {
            emit_paragraph_edge(s, &self.audio_store, &self.batch_tx, sr, self.batch_jobs, on_event);
        }
        // 句级日志(debug):段落/段 id、音频时长、两路文本(异步 batch 尚未返回)、会话喂帧数。
        if let Some(s) = sentences.last() {
            debug!(
                paragraph_id = paragraph_id,
                sentence_id = s.id,
                time_ms = ((s.end_s - s.start_s) * 1000.0).round() as u64,
                fed,
                streaming = %s.streaming_text,
                "句定稿(句级 batch 稍后入队,异步执行)"
            );
        }
        // Final stream fragment: the sentence's DEFINITIVE streaming text (live partials only
        // decode up to the last throttle frame; finalize is authoritative).
        if let Some(s) = sentences.last().filter(|s| !s.streaming_text.is_empty()) {
            on_event(Stage1Event::StreamFragment {
                paragraph_id,
                sentence_id: s.id,
                text: s.streaming_text.clone(),
                at_s: end_s,
            });
        }
        // ★ 顺序不变式(竞态防护):先把 `Batch` 事件发上 stage2 通道,再入队句级 batch job。
        // 二者是不同 channel —— 若先入队 job,worker 可能在 `Batch` 被 Finalizer 处理前就产出
        // `SentenceBatchReady`,Finalizer 找不到该段条目而丢弃它,`ready` 永不达 `expected` →
        // 该段悬挂(永不就绪)。先发 `Batch`(占位建 pending)后入队 job,则结果必在 `Batch`
        // 之后落到同一条 stage2 FIFO 通道 → 就绪计数必到齐。
        on_event(Stage1Event::Batch { paragraph_id, sentences, sr });
        // 入队句级 batch job(非阻塞;worker 线程跑阻塞 recognize,结果回传
        // SentenceBatchReady)。发送失败 = batch worker 已死(极端故障)——记日志继续,
        // 该句 batch 缺失按 None 处理(best_text 回退流式)。
        // round12:`batch_jobs = false`(Pipeline 异步任务自管 batch)→ 不投递,
        // pipeline 在 `Batch` 事件处理时从 audio_store 取 clip 自建任务。
        if self.batch_jobs {
            let job = BatchJob::Sentence {
                paragraph_id,
                sentence_id,
                pcm: Arc::clone(&sentence_pcm),
                sr,
            };
            if let Err(e) = self.batch_tx.send(job) {
                warn!(error = %e, sentence_id, "batch worker gone — sentence batch job dropped");
            }
        }
    }
}

/// 下一次唤醒截止:最早的真实定时器,或 None(无定时 → 无限期挂起等音频)。
/// `flush_pending`:主动归档挂起中 → 最长 50ms 后醒来重试(EOS 一到立即归档,
/// 否则 condvar park 到 settle deadline 才醒,flush 延迟退化回 merge_gap)。
fn next_wake_at(
    tracker: &ParagraphTracker,
    sess: &Option<ActiveSession>,
    ring_empty_since: Option<Instant>,
    now_s: f64,
    speaking: bool,
    flush_pending: bool,
) -> Option<Duration> {
    let mut wake_at: Option<Duration> = None;
    if flush_pending {
        wake_at = Some(Duration::from_millis(50));
    }
    if let Some(d) = tracker.settle_deadline(now_s, speaking) {
        let d = Duration::from_secs_f64(d.max(0.05));
        wake_at = Some(wake_at.map_or(d, |w| w.min(d)));
    }
    if let Some(a) = sess.as_ref() {
        if !a.last_partial.is_empty() {
            let d = STALE_SESSION_RESET.saturating_sub(a.last_change.elapsed());
            wake_at = Some(wake_at.map_or(d, |w| w.min(d)));
        }
    }
    if let Some(since) = ring_empty_since {
        let has_partial = sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
        if has_partial {
            // Silence-feed deadline: force VAD EOS if the source dropped mid-utterance.
            let d = Duration::from_secs(2).saturating_sub(since.elapsed());
            wake_at = Some(wake_at.map_or(d, |w| w.min(d)));
        }
    }
    wake_at
}

impl Stage1Recognizer for OnnxStage1Recognizer {
    // R5 已整改(2026-08-30 batch 异步化): 轮询已除(ring 挂 Condvar,仅真实截止时间唤醒,
    // 空闲零唤醒);batch 调用移出消费线程 —— EOS/settle 只入队 BatchJob(微秒级),阻塞的
    // recognize 由 Pipeline 的 batch worker 线程执行,结果经 SentenceBatchReady /
    // ParagraphBatchReady 回传。消费循环不再被 batch 阻塞:流式/VAD/check_settle 持续运行,
    // 修复了"间隔 1–3.5s 首句被吞"(batch 阻塞期间墙钟越过 merge_gap 导致段落误切)。
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) {
        let sr = 16000u32;
        let start = Instant::now();
        let mut last_diag = Instant::now();
        let mut frames_in = 0u64;

        let sasr = self.mgr.streaming_asr();
        // 流式会话:段/段落边界重置,由 VAD detected() 门控喂帧。`cur_sentence` 由回溯式 SOS 分配
        // (与 EOS 同批到达),EOS 臂用它建句。
        let mut sess: Option<ActiveSession> = sasr.map(|asr| ActiveSession::new(asr.create_session()));
        let mut ring_empty_since: Option<Instant> = None;
        let mut tracker = ParagraphTracker::new(self.merge_gap_s);
        let mut cur_sentence: SentenceId = 0;
        let mut last_silence_feed = Instant::now(); // 断流喂静音的节流(100ms)
        let mut lead_in: VecDeque<Vec<i16>> = VecDeque::new(); // 起音补喂缓冲(~0.5s)
        let mut speech_active = false; // 上一帧 detected()——翻转时补喂 lead_in

        loop {
            // ⓪ idle 深度睡眠:running=false → 退出消费循环。daemon 断开 scout,下一个客户端
            //   连接时置回 true 并重新调用 run() 恢复识别。
            if !self.running.load(Ordering::Relaxed) {
                return;
            }
            // ① 连接开关:scout 暂停时挂起等音频,不做 VAD/ASR
            if !self.active.load(Ordering::Relaxed) {
                let _ = wait_frame(&self.ring, &self.ring_cv, WINDOW, None);
                continue;
            }

            // ② 时间驱动检查:主动归档 / 段落定稿 / 停滞看门狗 / 诊断
            let now_s = start.elapsed().as_secs_f64();
            // `speaking`(流式 partial 非空)抑制段落按墙钟定稿——回溯式 VAD 的下一句 SOS
            // 尚未到达,若定稿会把下一句错划进新段落。
            let speaking = sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
            // 用户侧主动归档(IME 分字符 = "我说完了"):跳过 merge_gap 剩余等待立即整段
            // batch。说话中(EOS 未到)保持挂起下一 tick 重试 —— 立即切段会截断尾音;
            // 无段落则消费掉标记(空按,不让陈旧 flush 影响之后的语音)。
            if self.flush_paragraph.load(Ordering::Acquire) && !speaking {
                match tracker.force_settle() {
                    Some(settled) => {
                        self.flush_paragraph.store(false, Ordering::Release);
                        info!(paragraph_id = settled.paragraph_id, sentences = settled.sentences.len(),
                            "flush: 主动归档(跳过 merge_gap 等待)");
                        emit_paragraph_edge(settled, &self.audio_store, &self.batch_tx, sr, self.batch_jobs, on_event);
                        sess = sasr.map(|asr| ActiveSession::new(asr.create_session())); // 段落边界重置会话
                    }
                    None if !tracker.has_open_paragraph() => {
                        self.flush_paragraph.store(false, Ordering::Release);
                    }
                    None => {} // 句进行中 → 挂起,等 EOS 后下一 tick 强制定稿
                }
            }
            if let Some(settled) = tracker.check_settle(now_s, speaking) {
                emit_paragraph_edge(settled, &self.audio_store, &self.batch_tx, sr, self.batch_jobs, on_event);
                sess = sasr.map(|asr| ActiveSession::new(asr.create_session())); // 段落边界重置会话
            }
            if let Some(a) = sess.as_ref() {
                if !a.last_partial.is_empty() && a.last_change.elapsed() >= STALE_SESSION_RESET {
                    warn!(stale_s = a.last_change.elapsed().as_secs(), partial = %a.last_partial,
                        "流式会话停滞重置——VAD 未定句的微弱音频不残留到下一句");
                    sess = sasr.map(|asr| ActiveSession::new(asr.create_session()));
                }
            }
            if last_diag.elapsed() >= Duration::from_secs(3) {
                let has_partial = sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
                debug!(frames = frames_in, ring = self.ring.lock().unwrap().len(), has_partial, "stage1 diag");
                last_diag = Instant::now();
            }

            // ③ 取帧:ring 有帧直接取;空则 park 等音频/截止(断流>2s 且有 partial → 喂静音逼 EOS)
            let wake_at = next_wake_at(
                &tracker,
                &sess,
                ring_empty_since,
                now_s,
                speaking,
                self.flush_paragraph.load(Ordering::Acquire),
            );
            let frame = match self.drain_frame(&mut ring_empty_since, &sess, &mut last_silence_feed, wake_at) {
                FrameResult::Frame(f) => f,
                FrameResult::Parked => continue,
            };
            frames_in += 1;

            // ④ VAD:每帧跑(便宜),得到 detected()(实时语音信号,门控流式) + 分句事件
            let vad = self.mgr.vad().unwrap();
            let events = vad.push_frame(&frame);
            let v_detected = vad.detected();

            // ⑤ 流式:VAD 门控喂帧/解码(空闲 park);起音补喂 lead_in(soft onset);
            //    accept 与 pcm 喂同一帧 → 流式/batch 共享音频
            self.feed_streaming(
                &mut sess, &mut tracker, &mut lead_in, &mut speech_active,
                &frame, sr, start.elapsed().as_secs_f64(), v_detected, on_event,
            );

            // ⑥ 分句:SOS 分配段号;EOS 定稿成段(batch + ParagraphEdge)
            for ev in events {
                match ev.kind {
                    VadEventKind::StartOfSpeech => cur_sentence = tracker.on_sos(),
                    VadEventKind::EndOfSpeech => {
                        let end_s = start.elapsed().as_secs_f64();
                        let a = sess.take();
                        let fed = a.as_ref().map(|a| a.fed).unwrap_or(0);
                        sess = sasr.map(|asr| ActiveSession::new(asr.create_session()));
                        self.finalize_sentence(
                            a, &mut tracker, &mut cur_sentence, sr, end_s, ev.pcm.clone(), fed, on_event,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentence(id: SentenceId, start_s: f64, end_s: f64) -> VadSentence {
        VadSentence {
            id,
            audio_id: id,
            start_s,
            end_s,
            streaming_text: format!("s{id}"),
            batch_text: Some(format!("b{id}")),
        }
    }

    #[test]
    fn short_gap_absorbs_into_same_paragraph() {
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos();
        let (settled, w1, sentences) = t.on_eos(sentence(s1, 0.0, 0.5));
        assert!(settled.is_none());
        assert_eq!(sentences.len(), 1);

        // gap 1.0−0.5 = 0.5 < 2.5 → same paragraph, second sentence (merge happens at EOS,
        // where the true onset is back-derived).
        let s2 = t.on_sos();
        let (settled, w, sentences) = t.on_eos(sentence(s2, 1.0, 1.5));
        assert!(settled.is_none(), "short gap must NOT settle");
        assert_eq!(w, w1, "same paragraph continues");
        assert_eq!(sentences.len(), 2, "both sentences in one paragraph");
    }

    #[test]
    fn big_gap_settles_previous_paragraph_and_opens_new_one() {
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos();
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // gap 5.0−0.5 = 4.5 ≥ 2.5 → settle w1 at the next sentence's EOS, open w2.
        let s2 = t.on_sos();
        let (settled, w2, sentences) = t.on_eos(sentence(s2, 5.0, 5.5));
        let s = settled.expect("big gap settles the previous paragraph");
        assert_eq!(s.paragraph_id, w1);
        assert_eq!(s.sentences.len(), 1);
        assert_ne!(w2, w1, "a fresh paragraph opens (random ids must differ)");
        assert_eq!(sentences.len(), 1);
    }

    #[test]
    fn settle_timeout_closes_trailing_paragraph() {
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos();
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        assert!(t.check_settle(2.0, false).is_none(), "2.0 − 0.5 = 1.5 < 2.5, not yet");
        let s = t.check_settle(3.0, false).expect("3.0 − 0.5 = 2.5 ≥ merge_gap → settle");
        assert_eq!(s.paragraph_id, w1);
        assert!(t.check_settle(10.0, false).is_none(), "nothing open anymore");
    }

    #[test]
    fn force_settle_skips_merge_gap_wait() {
        // 主动归档:远未到 merge_gap 也能立即关段(IME"我说完了"信号)。
        let mut t = ParagraphTracker::new(2.5);
        assert!(t.force_settle().is_none(), "无段落 → None(调用方消费掉 flush 标记)");
        assert!(!t.has_open_paragraph());
        let s1 = t.on_sos();
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // 0.2s 后强制归档(gap 0.2 < merge_gap 2.5 —— 常规定稿还早)。
        let s = t.force_settle().expect("有已定稿句 → 立即归档");
        assert_eq!(s.paragraph_id, w1);
        assert_eq!(s.sentences.len(), 1);
        assert!(!t.has_open_paragraph(), "段已关");
        assert!(t.check_settle(100.0, false).is_none(), "settle 路径不再重复触发");
        // 归档后再次 force → 无段落 → None。
        assert!(t.force_settle().is_none());
    }

    #[test]
    fn force_settle_holds_while_sentence_active() {
        // 句进行中(SOS 已见 EOS 未到)→ 不动,调用方保持 flush 挂起。
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos();
        let (_, _, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        let s2 = t.on_sos(); // 第二句开口
        assert!(t.force_settle().is_none(), "active 句压制强制归档");
        assert!(t.has_open_paragraph(), "段落仍在 → flush 保持挂起");
        let (_, w, sentences) = t.on_eos(sentence(s2, 1.0, 1.5));
        let s = t.force_settle().expect("EOS 落定后重试成功");
        assert_eq!(s.paragraph_id, w);
        assert_eq!(sentences.len(), 2);
    }

    #[test]
    fn settle_deadline_counts_down_to_merge_gap() {
        // The condvar wake deadline: exactly when check_settle would fire (consumes loop
        // parks on the ring condvar instead of polling — this is its only wake source for
        // the trailing paragraph).
        let mut t = ParagraphTracker::new(2.5);
        assert!(t.settle_deadline(0.0, false).is_none(), "nothing open yet");
        let s1 = t.on_sos();
        t.on_eos(sentence(s1, 0.0, 0.5));
        assert!((t.settle_deadline(1.0, false).unwrap() - 2.0).abs() < 1e-9, "2.5 − (1.0 − 0.5)");
        assert!((t.settle_deadline(3.0, false).unwrap() - 0.0).abs() < 1e-9, "due now, clamped at 0");
        let _s2 = t.on_sos(); // sentence in progress (active=true)
        assert!(t.settle_deadline(1.2, false).is_none(), "active sentence ⇒ suppressed, no deadline");
    }

    #[test]
    fn active_sentence_suppresses_settle_timeout() {
        // Regression guard: a long following sentence must not be mistaken for "no
        // continuation" and force-split the paragraph mid-speech.
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos();
        t.on_eos(sentence(s1, 0.0, 0.5));
        let _s2 = t.on_sos(); // sentence in progress (active=true)
        assert!(t.check_settle(100.0, false).is_none(), "active sentence ⇒ settle suppressed");
    }

    #[test]
    fn speaking_suppresses_settle_waiting_for_retroactive_sos() {
        // 回溯式 VAD 的回归防护:下一句的 SOS 要等它的 EOS 才到——在它到达前,流式
        // session 的 partial 非空(=speaking=true)必须抑制 settle 超时。否则墙钟超时
        // 会在下一句说话时定稿,把它错划进新段落(症状:段落永远只有 1 个 sentence)。
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos();
        t.on_eos(sentence(s1, 0.0, 0.5));
        // 下一句正在说话(SOS 尚未到),墙钟已远超 merge_gap —— speaking=true 抑制。
        assert!(t.check_settle(100.0, true).is_none(), "speaking ⇒ settle suppressed");
        assert!(t.settle_deadline(100.0, true).is_none(), "speaking ⇒ no settle deadline");
        // 说话停止(speaking=false)后,同一时刻立刻能定稿。
        assert!(t.check_settle(100.0, false).is_some(), "not speaking ⇒ settle fires");
    }

    #[test]
    fn merge_gap_zero_makes_every_sentence_its_own_paragraph() {
        let mut t = ParagraphTracker::new(0.0);
        let s1 = t.on_sos();
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // Any gap ≥ 0 settles at the next sentence's EOS (gap 0.6 − 0.5 = 0.1 ≥ 0).
        let s2 = t.on_sos();
        let (settled, w2, _) = t.on_eos(sentence(s2, 0.6, 0.7));
        assert_eq!(settled.expect("gap 0.1 ≥ 0 settles").paragraph_id, w1);
        assert_ne!(w2, w1);
        // …and the settle timeout fires immediately after an EOS too.
        let s3 = t.on_sos();
        t.on_eos(sentence(s3, 10.0, 10.5));
        assert!(t.check_settle(10.5, false).is_some(), "now − end = 0 ≥ 0 → settle");
    }

    fn sentence_into(store: &AudioStore, id: SentenceId) -> VadSentence {
        VadSentence {
            id,
            // 异步 batch: 消费循环入队时句的 batch_text 恒 None(结果经 SentenceBatchReady 回传)。
            audio_id: store.insert(Arc::new(vec![1i16; 1600])),
            start_s: id as f64,
            end_s: id as f64 + 0.1,
            streaming_text: format!("流式{id}"),
            batch_text: None,
        }
    }

    #[test]
    fn single_sentence_paragraph_enqueues_no_rerun_job() {
        let store = AudioStore::new(1_000_000);
        let (tx, rx) = mpsc::channel();
        let settled = SettledParagraph {
            paragraph_id: 1,
            sentences: vec![sentence_into(&store, 1)],
        };
        let mut events = Vec::new();
        emit_paragraph_edge(settled, &store, &tx, 16000, true, &mut |ev| events.push(ev));
        assert!(rx.try_recv().is_err(), "单句段落绝不投递重跑 job(复用句级 batch)");
        match &events[0] {
            Stage1Event::ParagraphEdge { paragraph, .. } => assert_eq!(
                paragraph.batch_text.as_deref(),
                None,
                "异步模式: 事件时刻 batch_text 恒 None(单句复用句级结果,无 ParagraphBatchReady)"
            ),
            other => panic!("expected ParagraphEdge, got {other:?}"),
        }
    }

    #[test]
    fn multi_sentence_paragraph_enqueues_one_rerun_job() {
        let store = AudioStore::new(1_000_000);
        let (tx, rx) = mpsc::channel();
        let settled = SettledParagraph {
            paragraph_id: 1,
            sentences: vec![sentence_into(&store, 1), sentence_into(&store, 2)],
        };
        let mut events = Vec::new();
        emit_paragraph_edge(settled, &store, &tx, 16000, true, &mut |ev| events.push(ev));
        // 多句段落恰好投递一次重跑 job,携拼接后的整段 PCM(1600*2)。
        match rx.try_recv() {
            Ok(BatchJob::Paragraph { paragraph_id, pcm, sr }) => {
                assert_eq!(paragraph_id, 1);
                assert_eq!(pcm.len(), 3200, "job 携整段拼接 PCM");
                assert_eq!(sr, 16000);
            }
            other => panic!("expected exactly one Paragraph job, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "只投递一次");
        match &events[0] {
            Stage1Event::ParagraphEdge { paragraph, .. } => assert_eq!(
                paragraph.batch_text.as_deref(),
                None,
                "异步模式: 事件时刻 batch_text 恒 None(结果经 ParagraphBatchReady)"
            ),
            other => panic!("expected ParagraphEdge, got {other:?}"),
        }
    }

    #[test]
    fn empty_paragraph_emits_nothing_and_sends_no_job() {
        let store = AudioStore::new(1_000_000);
        let (tx, rx) = mpsc::channel();
        let settled = SettledParagraph { paragraph_id: 1, sentences: vec![] };
        let mut events = Vec::new();
        emit_paragraph_edge(settled, &store, &tx, 16000, true, &mut |ev| events.push(ev));
        assert!(events.is_empty(), "空段落不 emit 事件");
        assert!(rx.try_recv().is_err(), "空段落不投递 job");
    }

}
