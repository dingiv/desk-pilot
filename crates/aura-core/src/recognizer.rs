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
//! Boundary paradigm (docs/aura/stages.md): a paragraph OPENS at the VAD speech onset
//! (detected() rising edge → timestamp id, live partials carry the REAL key from the first
//! fragment); the VAD gap (`min_silence`) closes a [`VadSentence`] (its own streaming session
//! per D1 + one batch pass, packed as a `Batch` event with `batch_text: None`); the merge
//! paragraph (`merge_gap`) closes a [`VadParagraph`] (packed as a `ParagraphEdge` with
//! `batch_text: None`). PCM lives in the [`AudioStore`] by id as a shared `Arc<Vec<i16>>` —
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc as t_mpsc, oneshot, Notify};

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
use crate::{
    AudioId, ParagraphId, SentenceId, Stage1Event, VadEventKind, VadParagraph, VadSentence,
};
// ONNX 语音栈在 dp-models(feature `speech`)——audio-aura 不再直接依赖 sherpa-onnx。
use dp_models::onnx::{
    AsrBackend, AsrConfig, OnnxRuntimeManager, StreamingAsrConfig, StreamingSession, VadConfig,
    WINDOW,
};
use dp_models::{http::HttpAsr, AsrProvider, ProviderKind};

/// Default ring capacity: 10 min @ 16 kHz mono (~19 MB).
const DEFAULT_RING_CAP: usize = 16_000 * 600;
/// Streaming-partial decode cadence: every N paragraphs (~0.3s @ 32ms Silero paragraphs).
const PARTIAL_EVERY_FRAMES: u32 = 9;
/// Stale-session watchdog: reset the streaming session when its partial has been UNCHANGED
/// this long AND no EOS came — that means VAD never latched (audio below `threshold` =
/// discard-by-design), and its residue (hallucinated repetitions included) must NOT leak
/// into whatever sentence closes next (2026-08-17 实测:35s 悬置会话把上一句幻觉文本卷进
/// 下一句). Real speech never trips this: a ≥min_silence pause closes the sentence via EOS,
/// which resets the session long before the partial could go stale.
const STALE_SESSION_RESET: Duration = Duration::from_secs(8);
/// 起音→首条 partial 的盲区边际:partial 每 15 帧(~0.5s)才解码一次,起音后这段
/// 盲区里 `partial 非空` 还没翻转,但 VAD `detected()` 已经是 true —— settle 判定若
/// 只看 partial,起音落在 merge_gap 截止点前盲区里的下一句会被**误切**(段落本该
/// 合并;且关段后仍产生该段的 SF,客户端首选回落陈旧流式 = "batch 后退回流式"
/// 的 round15 回归)。0.6s = 0.3s 节流 + 起音补喂/解码余量。
const VOICE_SETTLE_MARGIN: f64 = 0.6;

/// settle 抑制的"说话中"判定:partial 非空,**或**最近一帧 VAD detected() 距今
/// < [`VOICE_SETTLE_MARGIN`](self::VOICE_SETTLE_MARGIN)。
fn speech_pending(partial_nonempty: bool, last_voice_s: f64, now_s: f64) -> bool {
    partial_nonempty || (now_s - last_voice_s) < VOICE_SETTLE_MARGIN
}

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
    pub fn with_remote_asr(
        mut self,
        endpoint: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        self.asr_kind = ProviderKind::Remote {
            endpoint: endpoint.into(),
            model: model.into(),
        };
        self
    }
}

// A Stage1 recognizer: audio in → [`Stage1Event`]s out. `run`(见下方 impl)是**原生
// 异步**的(round14b:帧等待走 `tokio::sync::Notify`;round21:流式解码独立 tokio::task)
// —— 调用方 `s1.run(cb).await`(永不完成直到 `running` 置 false)。round21b:固有
// `async fn`(原 `Stage1Recognizer` trait 单实现、无人 dyn/泛型用,已删;Send 由内部
// 状态自然满足,`tokio::spawn` 无碍)。

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
/// [`OnnxRuntimeManager`]). round21 后内部只起**一个**任务:流式识别 worker
/// ([`run_stream_worker`],async fn,`tokio::spawn` 交 executor 协作调度)。另两个阻塞
/// 作业(ingest / consume
/// loop / batch worker) are exposed as methods the `Pipeline` runs on threads it owns. The ring
/// is shared with the ingest thread; the consume loop runs on its caller's thread; batch jobs
/// are handed to the worker via an mpsc channel (see [`Self::new`]).
pub struct OnnxStage1Recognizer {
    mgr: Arc<OnnxRuntimeManager>,
    /// Batch ASR as a trait object: local OnnxAsr (from `mgr`) or remote HttpAsr. Streaming/VAD
    /// stay in `mgr` (always local sherpa).
    batch_asr: Arc<dyn AsrProvider>,
    ring: Arc<Mutex<AudioRing>>,
    /// Wakes the async consume loop when the ingest pushes frames (no polling).
    /// Notify 的 permit 语义天然防丢唤醒:push 后的 notify_one 会存一张 permit,
    /// 稍后的 `notified().await` 立即返回;且 notify_one 可从同步代码调用
    /// (ingest 仍是阻塞桥)。
    ring_notify: Arc<Notify>,
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
    /// Build models from `cfg` and warm them. 只加载模型不spawn任务 —— 唯一的内部任务
    /// (流式 worker)由 [`Self::run`] 每次进入时起、退出时随通道关闭而终(支持 idle 深睡
    /// 后重复 run)。Returns the recognizer + the RECV end of the batch-job
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
        let ring_notify = Arc::new(Notify::new());
        let (batch_tx, batch_rx) = mpsc::channel();
        Ok((
            Self {
                mgr,
                ring,
                ring_notify,
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
    pub fn new_with_mgr(
        mgr: Arc<OnnxRuntimeManager>,
        cfg: Stage1Config,
    ) -> Result<(Self, mpsc::Receiver<BatchJob>)> {
        let batch_asr: Arc<dyn AsrProvider> = match (&cfg.asr_kind, cfg.batch_enabled) {
            (_, false) => Arc::new(DisabledAsr),
            (ProviderKind::Local, _) => {
                Arc::clone(mgr.asr().expect("local mgr must carry the batch ASR"))
                    as Arc<dyn AsrProvider>
            }
            (ProviderKind::Remote { endpoint, model }, _) => {
                Arc::new(HttpAsr::new(endpoint.clone(), model.clone()))
            }
        };
        let ring = Arc::new(Mutex::new(AudioRing::new(cfg.ring_cap_samples)));
        let ring_notify = Arc::new(Notify::new());
        let (batch_tx, batch_rx) = mpsc::channel();
        Ok((
            Self {
                mgr,
                ring,
                ring_notify,
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
        let src = ScoutAudioSource::with_active(
            self.scout_addr.clone(),
            WINDOW,
            Arc::clone(&self.active),
        )
        .with_chunk_ms(self.scout_chunk_ms);
        src.stream(
            move |win| {
                let mut g = self.ring.lock().unwrap();
                g.push(win);
                drop(g);
                // Wake the async consume loop — it awaits the Notify between frames
                // (deadline-driven, no polling). notify_one 可从同步代码调用。
                self.ring_notify.notify_one();
            },
            Duration::from_secs(2),
        )
    }

    /// Run the blocking batch ASR **once** (no retry — real-time: a hang/timeout/circuit-trip
    /// must NOT stall finalization). `Ok(non-empty)` → `Some`; `Ok(empty)` (noise/silence) and
    /// `Err` (timeout / 断链 / 熔断) → `None` (caller falls back to streaming). The timeout +
    /// 断链熔断 live in [`dp_models::http::HttpAsr`] (ASR_TIMEOUT=3s, 断链即窗口内不发送);
    /// this just surfaces the outcome + logs the failure reason so "丢 batch" is diagnosable.
    pub fn recognize_once(
        &self,
        pcm: &[i16],
        sr: u32,
        what: &str,
        paragraph_id: ParagraphId,
    ) -> Option<String> {
        match self.batch_asr.recognize(pcm, sr) {
            Ok(text) if !text.trim().is_empty() => Some(text),
            Ok(_) => {
                debug!(
                    what,
                    paragraph_id, "batch 识别成功但文本为空(噪声/静音)→ 回退流式"
                );
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
    pub fn run_batch_worker(
        &self,
        rx: mpsc::Receiver<BatchJob>,
        on_result: &mut dyn FnMut(BatchJobResult),
    ) {
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
                BatchJob::Paragraph {
                    paragraph_id,
                    pcm,
                    sr,
                } => {
                    let t0 = Instant::now();
                    let text = self.recognize_once(&pcm, sr, "段落级重跑", paragraph_id);
                    let asr_ms = t0.elapsed().as_millis() as u64;
                    debug!(
                        paragraph_id,
                        asr_ms,
                        batch = text.as_deref().unwrap_or("(none)"),
                        "段落级 batch 重跑完成(异步 worker)"
                    );
                    on_result(BatchJobResult::Paragraph {
                        paragraph_id,
                        text,
                        asr_ms,
                    });
                }
            }
        }
    }
}

/// Wait until a full Silero frame is available in the ring (wakes on the ingest's
/// `Notify`). `timeout: Some` additionally caps the wait — `None` return means the deadline
/// fired (the caller re-runs its time-based checks); `timeout: None` parks until audio
/// arrives (no timer at all — nothing time-based is pending).
///
/// **async(round14b)**:消费循环原生异步 —— 唤醒源从 Condvar 换成 `tokio::sync::Notify`
/// (permit 语义:检查 ring 之后、`await` 之前的 push 不会丢唤醒 —— notify_one 存的
/// permit 会让 `notified()` 立即就绪)。std Mutex 保留:临界区是纳秒级的 ring 操作,
/// async 里短暂持锁是标准做法。
async fn wait_frame(
    ring: &Mutex<AudioRing>,
    notify: &Notify,
    frame_samples: usize,
    timeout: Option<Duration>,
) -> Option<Vec<i16>> {
    {
        let mut g = ring.lock().unwrap();
        if g.has_frame(frame_samples) {
            return Some(g.drain(frame_samples));
        }
    }
    // 先注册 waiter 再复查一次 ring(双保险;permit 语义本身已防丢唤醒)。
    let notified = notify.notified();
    {
        let mut g = ring.lock().unwrap();
        if g.has_frame(frame_samples) {
            return Some(g.drain(frame_samples));
        }
    }
    match timeout {
        Some(t) => {
            let _ = tokio::time::timeout(t, notified).await;
        }
        None => notified.await,
    }
    // 醒来(通知或截止)→ 终检一次 ring(截止竞态窗口内可能刚 push)。
    let mut g = ring.lock().unwrap();
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
/// EOS pending). The in-progress sentence's id/timing live recognizer-side(消费循环 + 流式
/// 任务);
/// the tracker only needs "is one active" for settle suppression. `opened_at` = 起音开段时刻
/// (VAD rising edge),供空段落 GC(起音后从未出句的微弱音频,静默满 merge_gap 即弃)。
struct OpenParagraph {
    paragraph_id: ParagraphId,
    sentences: Vec<VadSentence>,
    active: bool,
    opened_at: f64,
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
        Self {
            merge_gap_s,
            next_sentence_id: 1,
            last_win_id: 0,
            open: None,
        }
    }

    /// 生成段落 id = **创建时刻时间戳**(UNIX_EPOCH 起微秒,u64)。严格递增:
    /// `max(now, last+1)` 防时钟回拨/同微秒碰撞;恒 ≠ 0。
    ///
    /// **id 即顺序(契约)**:客户端 Transcript 以 id 排序(BTreeMap 降序 = 说话顺序),
    /// 时间戳天然单调,取代旧随机器(`next_random_win_id` —— 随机 id 打碎了客户端
    /// 排序假设,§7-A);时间戳还让 id 在日志里可直接读出段落创建时刻。
    fn next_win_id(&mut self) -> ParagraphId {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let mut id = now.max(self.last_win_id.saturating_add(1));
        if id == 0 {
            id = 1;
        }
        self.last_win_id = id;
        id
    }

    /// VAD 起音(detected() false→true 翻转)即开段 —— **真键前置**(§7-B 幽灵段根治):
    /// 段落 id 在说话第一刻就分配,live partial 从第一条起携带真实段键;不再依赖
    /// 回溯 SOS(EOS 时刻)补开。已有开段(段内第 2+ 句)则不动。
    fn on_speech_onset(&mut self, now: f64) {
        if self.open.is_none() {
            let id = self.next_win_id();
            self.open = Some(OpenParagraph {
                paragraph_id: id,
                sentences: Vec::new(),
                active: false,
                opened_at: now,
            });
        }
    }

    /// VAD StartOfSpeech. NOTE: the SOS is RETROACTIVE — it fires at the sentence's EOS instant
    /// (its wall-clock IS the EOS time, NOT the speech onset), so the merge/split decision
    /// CANNOT happen here (using the EOS instant as the onset would inflate every gap by the
    /// sentence's own duration and settle on EVERY sentence — the "paragraph never has >1 sentence"
    /// bug). Normally the paragraph was already opened at the speech onset
    /// ([`Self::on_speech_onset`]); the open here is only a degenerate fallback (no rising edge
    /// was ever seen). This allocates the sentence id + marks the sentence active; the settle
    /// decision moves to [`Self::on_eos`], which back-derives the true speech onset from the PCM.
    fn on_sos(&mut self, now: f64) -> SentenceId {
        if self.open.is_none() {
            let id = self.next_win_id();
            self.open = Some(OpenParagraph {
                paragraph_id: id,
                sentences: Vec::new(),
                active: false,
                opened_at: now,
            });
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
    fn on_eos(
        &mut self,
        sentence: VadSentence,
    ) -> (Option<SettledParagraph>, ParagraphId, Vec<VadSentence>) {
        let settled = self.settle_if_gap(sentence.start_s);
        if self.open.is_none() {
            // First sentence, or the previous paragraph just settled. opened_at 用回溯
            // onset(正常路径在起音已开段,这里是防御兜底)。
            let id = self.next_win_id();
            self.open = Some(OpenParagraph {
                paragraph_id: id,
                sentences: Vec::new(),
                active: false,
                opened_at: sentence.start_s,
            });
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
    ///
    /// 空段落 GC(起音开段的配套):开段后从未出句(微弱音频,partial 一直空)→ 静默满
    /// `merge_gap_s` 即**静默丢弃**(不发事件——emit 侧对空段落本就 no-op)。真语音不会
    /// 误伤:partial 自起音 ~0.5s 起非空 → `speaking` 抑制;句一旦落地(sentences 非空)
    /// 走正常 settle 路径。不 GC 的后果:陈旧空段被很久之后的语音复用,id(时间戳)
    /// 落后于中间段落 → 客户端 id 排序错位。
    fn check_settle(&mut self, now: f64, speaking: bool) -> Option<SettledParagraph> {
        let w = self.open.as_ref()?;
        if w.active || speaking {
            return None;
        }
        if w.sentences.is_empty() {
            if now - w.opened_at >= self.merge_gap_s {
                self.open = None; // 空段 GC:静默丢弃,无事件
            }
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
    /// settle: nothing open, a sentence in progress, or `speaking` — the next
    /// sentence's speech is ongoing but its SOS hasn't arrived yet). Drives the consume loop's
    /// condvar deadline — wake exactly when the trailing paragraph (or an empty onset-opened
    /// paragraph awaiting GC) is due, not on a poll cadence.
    fn settle_deadline(&self, now: f64, speaking: bool) -> Option<f64> {
        let w = self.open.as_ref()?;
        if w.active || speaking {
            return None;
        }
        if w.sentences.is_empty() {
            return Some((self.merge_gap_s - (now - w.opened_at)).max(0.0));
        }
        let last = w.sentences.last()?;
        Some((self.merge_gap_s - (now - last.end_s)).max(0.0))
    }

    fn take_open(&mut self) -> Option<SettledParagraph> {
        self.open.take().map(|w| SettledParagraph {
            paragraph_id: w.paragraph_id,
            sentences: w.sentences,
        })
    }

    /// The ids the sentence currently being spoken WILL get: the open paragraph's id (or the
    /// next one when nothing is open) + the next sentence id. Used to key live `StreamFragment`
    /// partials. 正常路径下段落已在起音开启(`on_speech_onset`),partial 从第一条起就是
    /// **真实段键**;`open=None` 的兜底预测仅剩退化场景(flush 在微弱音频中切段等),
    /// 实际不可达 —— partial 只在 detected 分支发射,而 rising edge 先于任何 accept 发生。
    /// Authoritative grouping arrives with the `Batch`/`ParagraphEdge` events.
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
    let streaming_text = settled
        .sentences
        .iter()
        .map(|s| s.streaming_text.as_str())
        .collect::<String>();
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
        debug!(
            paragraph_id = settled.paragraph_id,
            "单句段落——复用句级 batch,跳过整段重跑(不投递 job)"
        );
    } else if !batch_jobs {
        debug!(
            paragraph_id = settled.paragraph_id,
            "batch_jobs=false — 段落重跑由 pipeline 段任务自建(不投递 job)"
        );
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

/// The live streaming session + its partial-throttle state — **owned by the dedicated
/// streaming task** ([`run_stream_worker`]). D1 adaptation: sherpa's VAD emits SOS
/// RETROACTIVELY (together with EOS — the sentence only pops complete), so the session
/// CANNOT be created at speech onset. Instead it is fed CONTINUOUSLY and RESET at every
/// sentence boundary (EOS) and paragraph settle — each session therefore covers exactly
/// [previous boundary, this EOS] ≈ this one sentence (+ surrounding silence, which decodes
/// to nothing). Per-sentence attribution is preserved; live partials keep flowing.
struct ActiveSession {
    stream: StreamingSession,
    frames_since_partial: u32,
    last_partial: String,
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
            fed: 0,
            pcm: Vec::new(),
        }
    }
}

// ── round21:流式模型独立任务 ──────────────────────────────────────────────
// VAD 循环与流式解码彻底分任务:accept_waveform / decode_and_result(ONNX 前向,CPU
// 密集)不再与 VAD/分句/段落定稿共享执行流。帧经无界通道转发(音频速率 31 msg/s,
// B 处理快于实时,不积压);partial 回传后仍由消费循环发射 —— 两任务汇于同一事件
// 出口,顺序不变式(SF…→BS→PC/PCal)不破。唯一同步点:EOS 定稿(每句一次
// oneshot 往返,B 侧本地 finalize,几十 ms)。

/// VAD 循环 → 流式任务指令。
enum StreamCmd {
    /// 起音(rising edge):补喂 lead-in(soft onset 进会话),重置解码节拍。
    Onset { lead_in: Vec<Vec<i16>> },
    /// 语音帧(`detected()` 门控;断流时的合成静音帧同路)。
    Feed(Vec<i16>),
    /// 会话重置(段落边界 / 停滞看门狗)。
    Reset,
    /// EOS 定稿:B 侧 finalize_and_result,回执后自重置会话。
    Finalize { reply: oneshot::Sender<StreamFinal> },
}

/// finalize 回执。`pcm: None` = 流式未配置(调用方 fallback VAD 句)。
struct StreamFinal {
    text: String,
    pcm: Option<Vec<i16>>,
    fed: u32,
}

/// 流式任务回传:`text = Some(新 partial)` 仅在非空且变化时(→ 消费循环发射 SF);
/// `nonempty` = B 侧 last_partial 非空(speaking 抑制镜像)。
struct StreamOut {
    text: Option<String>,
    nonempty: bool,
}

/// B 侧 last_partial 状态的消费循环镜像(speaking 抑制 / 断流喂静音判据 / 停滞看门狗):
/// 每次 B 回传刷新;重置/定稿点由本侧直接清零(确定性,无竞态)。
#[derive(Clone, Copy)]
struct PartialMirror {
    nonempty: bool,
    last_change: Instant,
}

impl PartialMirror {
    fn empty() -> Self {
        Self {
            nonempty: false,
            last_change: Instant::now(),
        }
    }
}

/// 流式识别任务**本体**(round21:async fn)。由消费循环侧 `tokio::spawn` 交出去 ——
/// executor 协作调度(ONNX 前向几十 ms 量级,标准协作负载),**不占阻塞线程**。
/// cmd sender drop(消费循环退出)即任务结束。
async fn run_stream_worker(
    mgr: Arc<OnnxRuntimeManager>,
    sr: u32,
    mut cmd_rx: t_mpsc::UnboundedReceiver<StreamCmd>,
    out_tx: t_mpsc::UnboundedSender<StreamOut>,
) {
    let Some(asr) = mgr.streaming_asr() else {
        // 流式未配置:与旧内联行为一致——全部 no-op;finalize 回空定稿
        // (pcm: None → 调用方 fallback VAD 句)。
        while let Some(cmd) = cmd_rx.recv().await {
            if let StreamCmd::Finalize { reply } = cmd {
                let _ = reply.send(StreamFinal {
                    text: String::new(),
                    pcm: None,
                    fed: 0,
                });
            }
        }
        return;
    };
    let mut a = ActiveSession::new(asr.create_session());
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            StreamCmd::Onset { lead_in } => {
                for chunk in &lead_in {
                    a.stream.accept_waveform(sr as i32, chunk);
                    a.pcm.extend_from_slice(chunk);
                    a.fed += 1;
                }
                a.frames_since_partial = 0; // 补喂后重新起解码节拍
            }
            StreamCmd::Feed(f) => {
                a.stream.accept_waveform(sr as i32, &f);
                a.pcm.extend_from_slice(&f); // 流式与 batch 共用同一句音频
                a.fed += 1;
                a.frames_since_partial += 1;
                if a.frames_since_partial >= PARTIAL_EVERY_FRAMES {
                    let partial = asr.decode_and_result(&a.stream);
                    let changed = !partial.is_empty() && partial != a.last_partial;
                    if changed {
                        a.last_partial = partial.clone();
                    }
                    let _ = out_tx.send(StreamOut {
                        text: changed.then_some(partial),
                        nonempty: !a.last_partial.is_empty(),
                    });
                    a.frames_since_partial = 0;
                }
            }
            StreamCmd::Reset => a = ActiveSession::new(asr.create_session()),
            StreamCmd::Finalize { reply } => {
                let text = asr.finalize_and_result(&a.stream);
                let fin = StreamFinal {
                    text,
                    pcm: Some(std::mem::take(&mut a.pcm)),
                    fed: a.fed,
                };
                let _ = reply.send(fin); // 回执失败 = 循环已退出,会话随之丢弃
                a = ActiveSession::new(asr.create_session());
            }
        }
    }
}

/// 冲刷流式任务回传:`text` 变化 → 发射 `StreamFragment`;镜像刷新(speaking 抑制 /
/// 停滞看门狗 / 断流喂静音判据)。partial 变化时刻 = 镜像刷新时刻(与旧内联语义一致)。
fn drain_stream_out(
    stream_rx: &mut t_mpsc::UnboundedReceiver<StreamOut>,
    tracker: &ParagraphTracker,
    at_s: f64,
    mirror: &mut PartialMirror,
    on_event: &mut dyn FnMut(Stage1Event),
) {
    while let Ok(out) = stream_rx.try_recv() {
        if out.text.is_some() {
            mirror.last_change = Instant::now(); // partial 变化时刻 = 镜像刷新时刻
        }
        if let Some(text) = out.text {
            let (paragraph_id, sentence_id) = tracker.prospective();
            on_event(Stage1Event::StreamFragment {
                paragraph_id,
                sentence_id,
                text,
                at_s,
            });
        }
        mirror.nonempty = out.nonempty;
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
    async fn drain_frame(
        &self,
        ring_empty_since: &mut Option<Instant>,
        partial_nonempty: bool,
        last_silence_feed: &mut Instant,
        wake_at: Option<Duration>,
    ) -> FrameResult {
        // 作用域块取帧:guard 绝不跨 await(generator Send 分析对显式 drop 保守,
        // 作用域块是可靠写法)。
        let ready = {
            let mut g = self.ring.lock().unwrap();
            g.has_frame(WINDOW).then(|| g.drain(WINDOW))
        };
        if let Some(f) = ready {
            *ring_empty_since = None;
            return FrameResult::Frame(f);
        }
        ring_empty_since.get_or_insert_with(Instant::now);
        let since = *ring_empty_since.as_ref().unwrap();
        let has_partial = partial_nonempty;
        if since.elapsed() > Duration::from_secs(2) && has_partial {
            // 断流:喂合成静音让 VAD 发 EOS(每 100ms 至多一帧,~1s 静音约 3s 墙钟)
            if last_silence_feed.elapsed() >= Duration::from_millis(100) {
                *last_silence_feed = Instant::now();
                debug!("ring empty > 2s during speech — feeding silence to force VAD EOS");
                FrameResult::Frame(vec![0i16; WINDOW])
            } else {
                match wait_frame(
                    &self.ring,
                    &self.ring_notify,
                    WINDOW,
                    Some(Duration::from_millis(100)),
                )
                .await
                {
                    Some(f) => {
                        *ring_empty_since = None;
                        FrameResult::Frame(f)
                    }
                    None => FrameResult::Parked,
                }
            }
        } else {
            // Park until the ingest pushes or the next deadline — 无轮询,空闲零唤醒.
            match wait_frame(&self.ring, &self.ring_notify, WINDOW, wake_at).await {
                Some(f) => {
                    *ring_empty_since = None;
                    FrameResult::Frame(f)
                }
                None => FrameResult::Parked,
            }
        }
    }

    /// 定稿一个 VAD 句(EOS 臂):流式任务回执(`StreamFinal`,在调用前已 oneshot 取回)
    /// → streaming_text,句 PCM 入 store(共享 `Arc`),**入队句级 batch job(异步——消费
    /// `Arc`),**入队句级 batch job(异步——消费循环不阻塞)**,emit `Batch`(`batch_text: None`)
    /// 及可能的 `ParagraphEdge`。`fallback_pcm` = 流式未配置时的 VAD edge-extended 句。
    ///
    /// 噪声句不再在 EOS 丢弃:batch 异步后 EOS 时刻只有流式文本,若流式空就丢弃,会丢掉
    /// "流式没听出、batch 能听出"的真实语音(吞句的另一形态)。空句无文本贡献,由段落折叠
    /// 自然吸收;停滞幻觉由 8s 看门狗在下一句前清掉。
    fn finalize_sentence(
        &self,
        stream: StreamFinal,
        tracker: &mut ParagraphTracker,
        cur_sentence: &mut SentenceId,
        sr: u32,
        end_s: f64,
        fallback_pcm: Vec<i16>,
        on_event: &mut dyn FnMut(Stage1Event),
    ) {
        // 句 PCM = 流式任务累积的完整音频(含句首 soft onset)——与流式听到的完全一致,
        // 区别只在 batch 一次整句听(大块)vs 流式逐帧听(小块)。`pcm: None` = 流式未
        // 配置 → fallback VAD edge-extended 句。`Arc`:store / batch job 共享同一份分配,零拷贝。
        let sentence_pcm: Arc<Vec<i16>> = Arc::new(stream.pcm.unwrap_or(fallback_pcm));
        let streaming_text = stream.text;
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
            emit_paragraph_edge(
                s,
                &self.audio_store,
                &self.batch_tx,
                sr,
                self.batch_jobs,
                on_event,
            );
        }
        // 句级日志(debug):段落/段 id、音频时长、两路文本(异步 batch 尚未返回)、会话喂帧数。
        if let Some(s) = sentences.last() {
            debug!(
                paragraph_id = paragraph_id,
                sentence_id = s.id,
                time_ms = ((s.end_s - s.start_s) * 1000.0).round() as u64,
                fed = stream.fed,
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
        on_event(Stage1Event::Batch {
            paragraph_id,
            sentences,
            sr,
        });
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
    mirror: PartialMirror,
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
    if mirror.nonempty {
        let d = STALE_SESSION_RESET.saturating_sub(mirror.last_change.elapsed());
        wake_at = Some(wake_at.map_or(d, |w| w.min(d)));
    }
    if let Some(since) = ring_empty_since {
        if mirror.nonempty {
            // Silence-feed deadline: force VAD EOS if the source dropped mid-utterance.
            let d = Duration::from_secs(2).saturating_sub(since.elapsed());
            wake_at = Some(wake_at.map_or(d, |w| w.min(d)));
        }
    }
    wake_at
}

impl OnnxStage1Recognizer {
    // R5 已整改(2026-08-30 batch 异步化): 轮询已除(ring 挂 Notify,仅真实截止时间唤醒,
    // 空闲零唤醒);batch 调用移出消费线程 —— EOS/settle 只发事件(微秒级),阻塞的
    // recognize 由 Pipeline 的句任务执行。消费循环不再被 batch 阻塞:流式/VAD/check_settle
    // 持续运行,修复了"间隔 1–3.5s 首句被吞"(batch 阻塞期间墙钟越过 merge_gap 导致段落误切)。
    // round14b:消费循环本体 async —— 帧等待 = Notify(park 空闲零唤醒),VAD(每 32ms,
    // 微秒级)与流式解码(0.3s 节流)内联在 executor 上,量级是协作式调度的标准负载。
    // round21:流式解码再拎出 —— accept/decode 全部移入独立 tokio::task(async fn,
    // executor 协作调度),消费循环只转发帧/指令、发射事件;VAD/分句/段落定稿从此与
    // 流式推理零共享。
    // round21b:RPITIT 结案 —— 固有 `async fn run`(原 trait 已删)。
    /// 跑消费循环直到 `running` 被置 false(idle 深度睡眠)→ 返回。daemon 恢复时重新调用。
    pub async fn run(&self, on_event: &mut (dyn FnMut(Stage1Event) + Send)) {
            let sr = 16000u32;
            let start = Instant::now();
            let mut last_diag = Instant::now();
            let mut frames_in = 0u64;

            // round21:流式模型拎出消费循环 —— 独立 tokio::task(async fn,executor 协作
            // 调度,不占阻塞线程)。本循环只转发帧/起音/重置/定稿指令;partial 回传后仍
            // 从这里发射,事件全序(partial 先于 Batch/ParagraphEdge)不变。VAD(每帧,
            // 先跑)从此不与流式解码抢同一个任务。`cur_sentence` 由回溯式 SOS 分配
            // (与 EOS 同批到达)。
            let (stream_tx, cmd_rx) = t_mpsc::unbounded_channel();
            let (out_tx, mut stream_rx) = t_mpsc::unbounded_channel();
            tokio::spawn(run_stream_worker(Arc::clone(&self.mgr), sr, cmd_rx, out_tx));
            let has_stream = self.mgr.streaming_asr().is_some();
            let mut mirror = PartialMirror::empty();
            let mut ring_empty_since: Option<Instant> = None;
            let mut tracker = ParagraphTracker::new(self.merge_gap_s);
            let mut cur_sentence: SentenceId = 0;
            let mut last_silence_feed = Instant::now(); // 断流喂静音的节流(100ms)
            let mut lead_in: VecDeque<Vec<i16>> = VecDeque::new(); // 起音补喂缓冲(~0.5s)
            let mut speech_active = false; // 上一帧 detected()——翻转时补喂 lead_in
                                           // 最近一帧 detected()=true 的墙钟(初始 -1 = 从未有语音)—— settle 抑制的
                                           // 起音盲区边际用。
            let mut last_voice_s: f64 = -1.0;

            loop {
                // ⓪ idle 深度睡眠:running=false → 退出消费循环。daemon 断开 scout,下一个客户端
                //   连接时置回 true 并重新调用 run() 恢复识别。
                if !self.running.load(Ordering::Relaxed) {
                    return;
                }
                // ① 连接开关:scout 暂停时挂起等音频,不做 VAD/ASR
                if !self.active.load(Ordering::Relaxed) {
                    let _ = wait_frame(&self.ring, &self.ring_notify, WINDOW, None).await;
                    continue;
                }

                // ② 时间驱动检查:主动归档 / 段落定稿 / 停滞看门狗 / 诊断
                let now_s = start.elapsed().as_secs_f64();
                // ②′ 冲刷流式任务回传:partial → 事件;镜像刷新(speaking/看门狗/断流判据)
                drain_stream_out(&mut stream_rx, &tracker, now_s, &mut mirror, on_event);
                // `speaking` 抑制段落按墙钟定稿——回溯式 VAD 的下一句 SOS 尚未到达,若
                // 定稿会把下一句错划进新段落。组合判定:partial 非空 **或** 起音盲区边际
                // 内(detected() 近期见过;见 VOICE_SETTLE_MARGIN)。
                let speaking = speech_pending(mirror.nonempty, last_voice_s, now_s);
                // 用户侧主动归档(IME 分字符 = "我说完了"):跳过 merge_gap 剩余等待立即整段
                // batch。说话中(EOS 未到)保持挂起下一 tick 重试 —— 立即切段会截断尾音;
                // 无段落则消费掉标记(空按,不让陈旧 flush 影响之后的语音)。
                if self.flush_paragraph.load(Ordering::Acquire) && !speaking {
                    match tracker.force_settle() {
                        Some(settled) => {
                            self.flush_paragraph.store(false, Ordering::Release);
                            info!(
                                paragraph_id = settled.paragraph_id,
                                sentences = settled.sentences.len(),
                                "flush: 主动归档(跳过 merge_gap 等待)"
                            );
                            emit_paragraph_edge(
                                settled,
                                &self.audio_store,
                                &self.batch_tx,
                                sr,
                                self.batch_jobs,
                                on_event,
                            );
                            let _ = stream_tx.send(StreamCmd::Reset); // 段落边界重置会话
                            mirror.nonempty = false;
                        }
                        None if !tracker.has_open_paragraph() => {
                            self.flush_paragraph.store(false, Ordering::Release);
                        }
                        None => {} // 句进行中 → 挂起,等 EOS 后下一 tick 强制定稿
                    }
                }
                if let Some(settled) = tracker.check_settle(now_s, speaking) {
                    emit_paragraph_edge(
                        settled,
                        &self.audio_store,
                        &self.batch_tx,
                        sr,
                        self.batch_jobs,
                        on_event,
                    );
                    let _ = stream_tx.send(StreamCmd::Reset); // 段落边界重置会话
                    mirror.nonempty = false;
                }
                if mirror.nonempty && mirror.last_change.elapsed() >= STALE_SESSION_RESET {
                    warn!(
                        stale_s = mirror.last_change.elapsed().as_secs(),
                        "流式会话停滞重置——VAD 未定句的微弱音频不残留到下一句"
                    );
                    let _ = stream_tx.send(StreamCmd::Reset);
                    mirror.nonempty = false;
                }
                if last_diag.elapsed() >= Duration::from_secs(3) {
                    let has_partial = mirror.nonempty;
                    debug!(
                        frames = frames_in,
                        ring = self.ring.lock().unwrap().len(),
                        has_partial,
                        "stage1 diag"
                    );
                    last_diag = Instant::now();
                }

                // ③ 取帧:ring 有帧直接取;空则 park 等音频/截止(断流>2s 且有 partial → 喂静音逼 EOS)
                let wake_at = next_wake_at(
                    &tracker,
                    mirror,
                    ring_empty_since,
                    now_s,
                    speaking,
                    self.flush_paragraph.load(Ordering::Acquire),
                );
                let frame = match self
                    .drain_frame(
                        &mut ring_empty_since,
                        mirror.nonempty,
                        &mut last_silence_feed,
                        wake_at,
                    )
                    .await
                {
                    FrameResult::Frame(f) => f,
                    FrameResult::Parked => continue,
                };
                frames_in += 1;

                // ④ VAD:每帧跑(便宜),得到 detected()(实时语音信号,门控流式) + 分句事件
                let vad = self.mgr.vad().unwrap();
                let events = vad.push_frame(&frame);
                let v_detected = vad.detected();
                if v_detected {
                    last_voice_s = start.elapsed().as_secs_f64();
                }

                // ⑤ 流式转发(VAD 门控;模型在独立任务):起音开段(A 侧 tracker)+
                //    补喂 lead_in(soft onset);语音帧经通道送 B accept+节流解码,partial
                //    回传由 ②′ 发射。accept 与 pcm 喂同一帧 → 流式/batch 共享音频。
                if has_stream {
                    if v_detected {
                        if !speech_active {
                            // ★ 起音即开段(§7-B 根治):rising edge 立刻分配真实段落 id ——
                            // 此后本段所有 partial/事件都携带真键,幽灵段(预测键)不复存在。
                            tracker.on_speech_onset(start.elapsed().as_secs_f64());
                            // 起音:补喂 lead-in,让流式/batch 都听到 soft onset
                            let _ = stream_tx.send(StreamCmd::Onset {
                                lead_in: lead_in.drain(..).collect(),
                            });
                        }
                        let _ = stream_tx.send(StreamCmd::Feed(frame));
                    } else {
                        // 空闲:流式会话 park;只累积有界 lead-in(供下次起音补喂)
                        lead_in.push_back(frame);
                        if lead_in.len() > LEAD_IN_FRAMES {
                            lead_in.pop_front();
                        }
                    }
                    speech_active = v_detected;
                }

                // ⑥ 分句:SOS 分配句号(段落已在起音开启,SOS 只补 sentence id);
                //    EOS 定稿成句(batch + ParagraphEdge)
                for ev in events {
                    match ev.kind {
                        VadEventKind::StartOfSpeech => {
                            cur_sentence = tracker.on_sos(start.elapsed().as_secs_f64())
                        }
                        VadEventKind::EndOfSpeech => {
                            let end_s = start.elapsed().as_secs_f64();
                            // 先冲刷在途 partial(B 的最后一次节流解码可能尚未回传),
                            // 再向流式任务要定稿(唯一同步点:每句一次,本地 finalize)。
                            drain_stream_out(
                                &mut stream_rx,
                                &tracker,
                                end_s,
                                &mut mirror,
                                on_event,
                            );
                            let (ftx, frx) = oneshot::channel();
                            let _ = stream_tx.send(StreamCmd::Finalize { reply: ftx });
                            mirror.nonempty = false; // 会话已被 B 取走重置
                            let stream = frx.await.unwrap_or(StreamFinal {
                                text: String::new(),
                                pcm: None,
                                fed: 0,
                            });
                            self.finalize_sentence(
                                stream,
                                &mut tracker,
                                &mut cur_sentence,
                                sr,
                                end_s,
                                ev.pcm.clone(),
                                on_event,
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

    // ── round15:起音盲区边际(speech_pending)──────────────────────────

    /// 起音→首条 partial 的 ~0.5s 盲区内,"partial 非空"还没翻转 —— settle 抑制必须
    /// 由 detected() 近期见过来兜住,否则起音落在 merge_gap 截止前的盲区里会被误切
    /// (客户端症状:首选 batch 后退回流式)。
    #[test]
    fn speech_pending_covers_partial_lag_after_onset() {
        // 起音 t=10.0(detected=true),首 partial ~10.5 才出。
        assert!(
            speech_pending(false, 10.0, 10.2),
            "盲区内:partial 未出也抑制"
        );
        assert!(speech_pending(false, 10.0, 10.55), "边缘仍覆盖");
        assert!(
            !speech_pending(false, 10.0, 10.7),
            "超出边际 → 不再抑制(可定稿/GC)"
        );
        // partial 到位后接管抑制;从未有语音则不抑制。
        assert!(speech_pending(true, 0.0, 100.0), "partial 非空恒抑制");
        assert!(!speech_pending(false, -1.0, 0.0), "从未检测到语音");
    }

    #[test]
    fn short_gap_absorbs_into_same_paragraph() {
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        let (settled, w1, sentences) = t.on_eos(sentence(s1, 0.0, 0.5));
        assert!(settled.is_none());
        assert_eq!(sentences.len(), 1);

        // gap 1.0−0.5 = 0.5 < 2.5 → same paragraph, second sentence (merge happens at EOS,
        // where the true onset is back-derived).
        let s2 = t.on_sos(0.0);
        let (settled, w, sentences) = t.on_eos(sentence(s2, 1.0, 1.5));
        assert!(settled.is_none(), "short gap must NOT settle");
        assert_eq!(w, w1, "same paragraph continues");
        assert_eq!(sentences.len(), 2, "both sentences in one paragraph");
    }

    #[test]
    fn big_gap_settles_previous_paragraph_and_opens_new_one() {
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // gap 5.0−0.5 = 4.5 ≥ 2.5 → settle w1 at the next sentence's EOS, open w2.
        let s2 = t.on_sos(0.0);
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
        let s1 = t.on_sos(0.0);
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        assert!(
            t.check_settle(2.0, false).is_none(),
            "2.0 − 0.5 = 1.5 < 2.5, not yet"
        );
        let s = t
            .check_settle(3.0, false)
            .expect("3.0 − 0.5 = 2.5 ≥ merge_gap → settle");
        assert_eq!(s.paragraph_id, w1);
        assert!(
            t.check_settle(10.0, false).is_none(),
            "nothing open anymore"
        );
    }

    #[test]
    fn force_settle_skips_merge_gap_wait() {
        // 主动归档:远未到 merge_gap 也能立即关段(IME"我说完了"信号)。
        let mut t = ParagraphTracker::new(2.5);
        assert!(
            t.force_settle().is_none(),
            "无段落 → None(调用方消费掉 flush 标记)"
        );
        assert!(!t.has_open_paragraph());
        let s1 = t.on_sos(0.0);
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // 0.2s 后强制归档(gap 0.2 < merge_gap 2.5 —— 常规定稿还早)。
        let s = t.force_settle().expect("有已定稿句 → 立即归档");
        assert_eq!(s.paragraph_id, w1);
        assert_eq!(s.sentences.len(), 1);
        assert!(!t.has_open_paragraph(), "段已关");
        assert!(
            t.check_settle(100.0, false).is_none(),
            "settle 路径不再重复触发"
        );
        // 归档后再次 force → 无段落 → None。
        assert!(t.force_settle().is_none());
    }

    #[test]
    fn force_settle_holds_while_sentence_active() {
        // 句进行中(SOS 已见 EOS 未到)→ 不动,调用方保持 flush 挂起。
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        let (_, _, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        let s2 = t.on_sos(0.0); // 第二句开口
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
        let s1 = t.on_sos(0.0);
        t.on_eos(sentence(s1, 0.0, 0.5));
        assert!(
            (t.settle_deadline(1.0, false).unwrap() - 2.0).abs() < 1e-9,
            "2.5 − (1.0 − 0.5)"
        );
        assert!(
            (t.settle_deadline(3.0, false).unwrap() - 0.0).abs() < 1e-9,
            "due now, clamped at 0"
        );
        let _s2 = t.on_sos(0.0); // sentence in progress (active=true)
        assert!(
            t.settle_deadline(1.2, false).is_none(),
            "active sentence ⇒ suppressed, no deadline"
        );
    }

    #[test]
    fn active_sentence_suppresses_settle_timeout() {
        // Regression guard: a long following sentence must not be mistaken for "no
        // continuation" and force-split the paragraph mid-speech.
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        t.on_eos(sentence(s1, 0.0, 0.5));
        let _s2 = t.on_sos(0.0); // sentence in progress (active=true)
        assert!(
            t.check_settle(100.0, false).is_none(),
            "active sentence ⇒ settle suppressed"
        );
    }

    #[test]
    fn speaking_suppresses_settle_waiting_for_retroactive_sos() {
        // 回溯式 VAD 的回归防护:下一句的 SOS 要等它的 EOS 才到——在它到达前,流式
        // session 的 partial 非空(=speaking=true)必须抑制 settle 超时。否则墙钟超时
        // 会在下一句说话时定稿,把它错划进新段落(症状:段落永远只有 1 个 sentence)。
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        t.on_eos(sentence(s1, 0.0, 0.5));
        // 下一句正在说话(SOS 尚未到),墙钟已远超 merge_gap —— speaking=true 抑制。
        assert!(
            t.check_settle(100.0, true).is_none(),
            "speaking ⇒ settle suppressed"
        );
        assert!(
            t.settle_deadline(100.0, true).is_none(),
            "speaking ⇒ no settle deadline"
        );
        // 说话停止(speaking=false)后,同一时刻立刻能定稿。
        assert!(
            t.check_settle(100.0, false).is_some(),
            "not speaking ⇒ settle fires"
        );
    }

    #[test]
    fn merge_gap_zero_makes_every_sentence_its_own_paragraph() {
        let mut t = ParagraphTracker::new(0.0);
        let s1 = t.on_sos(0.0);
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // Any gap ≥ 0 settles at the next sentence's EOS (gap 0.6 − 0.5 = 0.1 ≥ 0).
        let s2 = t.on_sos(0.6);
        let (settled, w2, _) = t.on_eos(sentence(s2, 0.6, 0.7));
        assert_eq!(settled.expect("gap 0.1 ≥ 0 settles").paragraph_id, w1);
        assert_ne!(w2, w1);
        // …and the settle timeout fires immediately after an EOS too.
        let s3 = t.on_sos(10.0);
        t.on_eos(sentence(s3, 10.0, 10.5));
        assert!(
            t.check_settle(10.5, false).is_some(),
            "now − end = 0 ≥ 0 → settle"
        );
    }

    // ── round13:起音即开段 + 时间戳 id(§7-A/B 修复)──────────────────────

    /// 起音开段 → prospective 返回**真实**段 id;该段后续所有事件(EOS 的
    /// Batch/ParagraphEdge)携带同一 id —— 幽灵段(预测键 ≠ 实际键)不复存在。
    #[test]
    fn onset_opens_paragraph_prospective_returns_real_id() {
        let mut t = ParagraphTracker::new(2.5);
        t.on_speech_onset(10.0);
        let (pid, _sid) = t.prospective();
        let s1 = t.on_sos(10.4);
        assert_eq!(t.prospective().0, pid, "段内 prospective 稳定");
        let (settled, w, _) = t.on_eos(sentence(s1, 10.0, 10.5));
        assert!(settled.is_none());
        assert_eq!(w, pid, "EOS 归属段 = 起音开的段(prospective 即真键)");
        // 静默满 merge_gap 关段,下一次起音 → 新段(时间戳更大)。
        let _ = t
            .check_settle(20.0, false)
            .expect("静默 9.5s ≥ 2.5s → settle");
        t.on_speech_onset(20.5);
        let (pid2, _) = t.prospective();
        assert!(pid2 > pid, "时间戳 id 严格递增 —— id 即顺序");
    }

    /// 时间戳 id 严格递增:同微秒连续开段(防御 max(last+1))也绝不重复/回退。
    #[test]
    fn timestamp_win_ids_strictly_increasing() {
        let mut t = ParagraphTracker::new(2.5);
        let mut prev = 0u64;
        for i in 0..8 {
            t.on_speech_onset(i as f64);
            let (pid, _) = t.prospective();
            assert!(pid > prev, "id 必须严格递增(时间戳,防时钟回拨/同微秒)");
            prev = pid;
            // 立刻出句并关段,下一轮开新段。
            let s = t.on_sos(i as f64);
            t.on_eos(sentence(s, i as f64, i as f64 + 0.5));
            let _ = t.check_settle(i as f64 + 10.0, false);
        }
    }

    /// 空段 GC:起音开的段从未出句(微弱音频)→ 静默满 merge_gap 静默丢弃;
    /// 不 GC 会让陈旧空段被很久之后的语音复用,id 落后 → 客户端排序错位。
    #[test]
    fn empty_onset_paragraph_gced_after_merge_gap() {
        let mut t = ParagraphTracker::new(2.5);
        t.on_speech_onset(0.0);
        let (pid, _) = t.prospective();
        assert!(t.check_settle(2.0, false).is_none(), "2.0 < 2.5,未到期");
        assert!(t.has_open_paragraph(), "GC 前段还在");
        assert!(
            t.check_settle(2.6, false).is_none(),
            "GC 静默:返回 None(无事件)"
        );
        assert!(!t.has_open_paragraph(), "空段静默满 merge_gap 即弃");
        // 下一次起音开**新**段(id 更大),不复用陈旧空段。
        t.on_speech_onset(100.0);
        let (pid2, _) = t.prospective();
        assert!(pid2 > pid, "新段时间戳更大");
        // settle_deadline 也覆盖空段(消费循环要能在 GC 时点醒来)。
        assert!(t.check_settle(103.0, false).is_none(), "GC 掉 100.0 的空段");
        assert!(!t.has_open_paragraph());
        t.on_speech_onset(200.0);
        let d = t.settle_deadline(201.0, false).expect("空段也有 GC 截止");
        assert!((d - 1.5).abs() < 1e-9, "2.5 − (201.0 − 200.0)");
    }

    /// 真语音不误伤:partial 非空(speaking)抑制空段 GC —— 长句(> merge_gap)
    /// 说话中不会被墙钟 GC 掉段落。
    #[test]
    fn speaking_suppresses_empty_onset_gc() {
        let mut t = ParagraphTracker::new(2.5);
        t.on_speech_onset(0.0);
        assert!(t.check_settle(100.0, true).is_none());
        assert!(t.has_open_paragraph(), "speaking ⇒ 空段不 GC");
        assert!(
            t.settle_deadline(100.0, true).is_none(),
            "speaking ⇒ 无 GC 截止"
        );
        assert!(t.check_settle(100.0, false).is_none(), "静默后 GC");
        assert!(!t.has_open_paragraph());
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
        assert!(
            rx.try_recv().is_err(),
            "单句段落绝不投递重跑 job(复用句级 batch)"
        );
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
            Ok(BatchJob::Paragraph {
                paragraph_id,
                pcm,
                sr,
            }) => {
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
        let settled = SettledParagraph {
            paragraph_id: 1,
            sentences: vec![],
        };
        let mut events = Vec::new();
        emit_paragraph_edge(settled, &store, &tx, 16000, true, &mut |ev| events.push(ev));
        assert!(events.is_empty(), "空段落不 emit 事件");
        assert!(rx.try_recv().is_err(), "空段落不投递 job");
    }
}
