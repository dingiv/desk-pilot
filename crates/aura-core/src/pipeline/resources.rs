//! resources —(原 recognizer.rs,round27 正名)Stage1 **资源/配置/生命周期层**:音频环 + 消费循环(`run`:VAD 喂帧 → 段落决策 →
//! 驱动流式/batch)与 batch worker。Owns ALL the consume-loop state, emitting
//! [`Stage1Event`]s — it does NOT touch files or run Stage2 (that's `pipeline`'s job,
//! `audio_aura_core::Pipeline`).
//!
//! round22 模块拆分:采音+检测+门控 = `vad.rs`(Stage0 前端,Stage0VAD/SileroVAD);
//! 消费循环(大脑)= `consume.rs`(顶层函数);分句/段落边界数学 = `tracker.rs`;
//! 流式识别任务 = `stream.rs`;本文件只剩资源/配置/生命周期。
//!
//! **本 crate 不创建任何线程** —— 阻塞工作全部以函数暴露,线程/任务由 `Pipeline` 创建并运行:
//! - [`Self::run_ingest`] — scout TCP → AudioRing(阻塞,自动重连);
//! - [`Self::run`] — 消费循环(VAD + 流式 + 边界决策),**永不被 batch 阻塞**(batch 由
//!   Pipeline 的任务结构经 [`Self::recognize_once`] 自建,round12 起)
//!   (batch 异步化的根因修复:见 docs/aura/debugging.md)。
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
//! let s1 = OnnxStage1Recognizer::new(Stage1Config::new(scout_addr))?;
//! let pipeline = Pipeline::new(s1, Box::new(stage2));
//! pipeline.run(running, resume, |ev| { /* TurnEvents */ });
//! ```

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use tokio::sync::Notify;
use std::sync::{Arc, Mutex};


use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::audio_store::{AudioStore, DEFAULT_CAP_SAMPLES};
use crate::pipeline::types::StreamCmd;
use crate::pipeline::types::{FrontBridge, FrontEvent};
use crate::pipeline::vad::{SileroVAD, Stage0VAD};
use tokio::sync::mpsc as t_mpsc;
use crate::ParagraphId;
// ONNX 语音栈在 dp-models(feature `speech`)——audio-aura 不再直接依赖 sherpa-onnx。
use dp_models::onnx::{
    AsrBackend, AsrConfig, OnnxRuntimeManager, StreamingAsrConfig, VadConfig,
};
use dp_models::{http::HttpAsr, AsrProvider, ProviderKind};

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
            scout_chunk_ms: None,
            asr_kind: ProviderKind::Local,
            merge_gap_s: 5.0,
            batch_enabled: true,
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

// A Stage1 recognizer: audio in → [`Stage1Event`]s out. 消费循环本体 = 顶层 async fn
// `consume::run(&s1, cb)`(consume.rs;round27 起不再以 impl 方法暴露——recognizer 只
// 留资源/配置/生命周期)。调用方永不返回直到 `running` 置 false(idle 深睡)。

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
/// loop / batch worker) are exposed as methods the `Pipeline` runs on threads it owns:
/// the front_q feeds the consume loop; the ingest thread owns VAD detection (R2) while the
/// mgr feeds ONLY the streaming worker (ASR).
pub struct OnnxStage1Recognizer {
    pub(crate) mgr: Arc<OnnxRuntimeManager>,
    /// Stage0 检测器(R1:trait 缝,SileroVAD 默认)—— mgr 从此只喂流式(ASR),
    /// VAD/ASR 在结构体层面分家。R2 起由拉流线程持有调用。
    pub(crate) vad: Arc<dyn Stage0VAD>,
    /// Batch ASR as a trait object: local OnnxAsr (from `mgr`) or remote HttpAsr. Streaming/VAD
    /// stay in `mgr` (always local sherpa).
    batch_asr: dp_models::AsyncAsr,
    /// Stage0 → 大脑的单 FIFO(R2 取代 AudioRing):ingest 逐帧 push FrontEvent
    /// (有界,满丢最旧 = 环回),消费循环 pop。frame/detected/events 同批同序。
    pub(crate) front_q: Arc<Mutex<VecDeque<FrontEvent>>>,
    /// Wakes the async consume loop when the ingest pushes FrontEvents (no polling).
    /// Notify 的 permit 语义天然防丢唤醒:push 后的 notify_one 会存一张 permit,
    /// 稍后的 `notified().await` 立即返回;且 notify_one 可从同步代码调用
    /// (ingest 仍是阻塞桥)。
    pub(crate) front_notify: Arc<Notify>,
    /// 共同时钟原点(D9):ingest 的 VAD 检测(last_voice_s)与消费循环的
    /// tracker/settle/onset 用同一把尺 —— run() 重入(idle 复醒)不换原点。
    pub(crate) start: Instant,
    /// 流式指令端口(R3):run() 入口安装 tx(随流式 worker 生命周期),退出摘除;
    /// 拉流线程经它直发 Onset/Feed(门控在前端),大脑持 clone 只发 Finalize/Reset。
    pub(crate) stream_port: Arc<Mutex<Option<t_mpsc::UnboundedSender<StreamCmd>>>>,
    /// partial 镜像的原子快照(R4):大脑每迭代刷新,拉流线程作断流喂静音判据。
    pub(crate) partial_live: Arc<AtomicBool>,
    /// Merge-paragraph gap (s) — see [`Stage1Config::merge_gap_s`].
    pub(crate) merge_gap_s: f64,
    pub(crate) active: Arc<AtomicBool>,
    /// idle 运行信号:false → run 退出循环(深度睡眠)。
    pub(crate) running: Arc<AtomicBool>,
    /// 主动归档信号(`Stage1Config::flush_paragraph`)—— run 循环消费,见下。
    pub(crate) flush_paragraph: Arc<AtomicBool>,
    /// The PCM store: sentences' clips live here by id (shared `Arc`) until their paragraph
    /// settles.
    pub(crate) audio_store: Arc<AudioStore>,
    /// scout 地址 — [`Self::run_ingest`] 用它建连接(ingest 线程由 Pipeline 创建)。
    scout_addr: String,
    /// 客户端请求 scout 的推流 cadence(ms)(`run_ingest` 用)。
    scout_chunk_ms: Option<u64>,
}

impl OnnxStage1Recognizer {
    /// Build models from `cfg` and warm them. 只加载模型不spawn任务 —— 唯一的内部任务
    /// (流式 worker)由 [`Self::run`] 每次进入时起、退出时随通道关闭而终(支持 idle 深睡
    /// 后重复 run)。round12 起 batch 由 Pipeline 的任务结构自管(recognize_once 直调),
    /// 本类型不再持有 batch job 通道。
    pub fn new(cfg: Stage1Config) -> Result<Self> {
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
        let batch_asr: dp_models::AsyncAsr = match (&cfg.asr_kind, cfg.batch_enabled) {
            (_, false) => dp_models::AsyncAsr::Blocking(Arc::new(DisabledAsr)),
            (ProviderKind::Local, _) => dp_models::AsyncAsr::Blocking(
                Arc::clone(mgr.asr().expect("local asr just loaded")) as Arc<dyn AsrProvider>,
            ),
            (ProviderKind::Remote { endpoint, model }, _) => {
                dp_models::AsyncAsr::Http(Arc::new(HttpAsr::new(endpoint.clone(), model.clone())))
            }
        };
        mgr.warm();
        let vad: Arc<dyn Stage0VAD> = Arc::new(SileroVAD::new(Arc::clone(&mgr)));
        let front_q = Arc::new(Mutex::new(VecDeque::new()));
        let front_notify = Arc::new(Notify::new());
        Ok(Self {
            mgr,
            vad,
            front_q,
            front_notify,
            start: Instant::now(),
            stream_port: Arc::new(Mutex::new(None)),
            partial_live: Arc::new(AtomicBool::new(false)),
            merge_gap_s: cfg.merge_gap_s,
            active: cfg.active,
            running: cfg.running,
            flush_paragraph: cfg.flush_paragraph,
            audio_store: Arc::new(AudioStore::new(DEFAULT_CAP_SAMPLES)),
            batch_asr,
            scout_addr: cfg.scout_addr.clone(),
            scout_chunk_ms: cfg.scout_chunk_ms,
        })
    }

    /// Use an already-loaded [`OnnxRuntimeManager`] (e.g. shared with another stage).
    pub fn new_with_mgr(mgr: Arc<OnnxRuntimeManager>, cfg: Stage1Config) -> Result<Self> {
        let batch_asr: dp_models::AsyncAsr = match (&cfg.asr_kind, cfg.batch_enabled) {
            (_, false) => dp_models::AsyncAsr::Blocking(Arc::new(DisabledAsr)),
            (ProviderKind::Local, _) => dp_models::AsyncAsr::Blocking(
                Arc::clone(mgr.asr().expect("local mgr must carry the batch ASR"))
                    as Arc<dyn AsrProvider>,
            ),
            (ProviderKind::Remote { endpoint, model }, _) => {
                dp_models::AsyncAsr::Http(Arc::new(HttpAsr::new(endpoint.clone(), model.clone())))
            }
        };
        let vad: Arc<dyn Stage0VAD> = Arc::new(SileroVAD::new(Arc::clone(&mgr)));
        let front_q = Arc::new(Mutex::new(VecDeque::new()));
        let front_notify = Arc::new(Notify::new());
        Ok(Self {
            mgr,
            vad,
            front_q,
            front_notify,
            start: Instant::now(),
            stream_port: Arc::new(Mutex::new(None)),
            partial_live: Arc::new(AtomicBool::new(false)),
            merge_gap_s: cfg.merge_gap_s,
            active: cfg.active,
            running: cfg.running,
            flush_paragraph: cfg.flush_paragraph,
            audio_store: Arc::new(AudioStore::new(DEFAULT_CAP_SAMPLES)),
            batch_asr,
            scout_addr: cfg.scout_addr.clone(),
            scout_chunk_ms: cfg.scout_chunk_ms,
        })
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

    /// Blocking ingest loop: omni-scout `/audio` (TCP) → [`AudioRing`] + Notify 唤醒,
    /// 自动重连(2s backoff),`active=false` 暂停连接。**Blocking —— Pipeline 在
    /// blocking 桥上运行**。循环本体在 VAD 模块([`crate::pipeline::vad::ingest_loop`]);
    /// 这里只做字段收集委托。
    pub fn run_ingest(&self) -> ! {
        crate::pipeline::front::ingest_loop(
            self.scout_addr.clone(),
            self.scout_chunk_ms,
            FrontBridge {
                vad: Arc::clone(&self.vad),
                has_stream: self.mgr.streaming_asr().is_some(),
                stream_port: Arc::clone(&self.stream_port),
                partial_live: Arc::clone(&self.partial_live),
                start: self.start,
                front_q: Arc::clone(&self.front_q),
                notify: Arc::clone(&self.front_notify),
                active: Arc::clone(&self.active),
            },
        )
    }

    /// Run the batch ASR **once** (no retry — real-time: a hang/timeout/circuit-trip
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
        surface_asr(self.batch_asr.recognize_sync(pcm, sr), what, paragraph_id)
    }

    /// [`Self::recognize_once`] 的异步轨(batch 任务用):远程 HttpAsr → 原生异步
    /// (超时/取消走连接池,无阻塞线程残留);本地 ONNX → `AsyncAsr` 内部的
    /// `spawn_blocking` 桥(CPU 密集推理的正确归宿)。回退/日志语义与同步版一致。
    pub async fn recognize_once_async(
        &self,
        pcm: &[i16],
        sr: u32,
        what: &str,
        paragraph_id: ParagraphId,
    ) -> Option<String> {
        surface_asr(self.batch_asr.recognize(pcm, sr).await, what, paragraph_id)
    }
}

/// recognize 结果 → 事件文本(`None` = 回退流式)+ 统一日志(两轨共享)。
fn surface_asr(res: anyhow::Result<String>, what: &str, paragraph_id: ParagraphId) -> Option<String> {
    match res {
        Ok(text) if !text.trim().is_empty() => Some(text),
        Ok(_) => {
            info!(
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

