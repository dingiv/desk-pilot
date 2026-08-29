//! Stage1Recognizer — encapsulates the Stage1 "noodle": the audio ring + omni-scout ingest
//! thread + Silero VAD + per-sentence streaming sessions + per-sentence batch passes + the
//! paragraph tracker. Owns ALL the loop state. It runs the consume loop internally and emits
//! [`Stage1Event`]s — it does NOT touch files or run Stage2 (that's `pipeline`'s job,
//! `audio_aura_core::Pipeline`).
//!
//! Boundary paradigm (docs/aura/stages.md): the VAD gap (`min_silence`) closes a
//! [`VadSentence`] (its own streaming session per D1 + one batch pass, packed as a `Batch`
//! event); the merge paragraph (`merge_gap`) closes a [`VadParagraph`] (concatenated PCM re-run
//! through the batch model, packed as a `ParagraphEdge` event). PCM lives in the
//! [`AudioStore`] by id — events carry ids + texts only, plus the paragraph's shared
//! `Arc<Vec<i16>>` assembled once at settle.
//!
//! ```ignore
//! let exec = OnnxStage1Recognizer::new(Stage1Config::new(scout_addr))?;
//! exec.run(&mut |ev| match ev {
//!     Stage1Event::StreamFragment { paragraph_id, sentence_id, text, .. } => println!("…{text}"),
//!     Stage1Event::Batch { paragraph_id, sentences } => stage2.calibrate_paragraph(paragraph_id, &sentences),
//!     Stage1Event::ParagraphEdge { paragraph } => stage2.calibrate_final(&paragraph),
//! });
//! ```

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
/// [`OnnxRuntimeManager`]). Thread-safe: the ring is shared with the ingest thread; the
/// consume loop runs on the caller's thread.
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
    /// The PCM store: sentences' clips live here by id until their paragraph settles.
    audio_store: Arc<AudioStore>,
}

impl OnnxStage1Recognizer {
    /// Build models from `cfg`, warm them, spawn the scout→ring ingest thread.
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
        spawn_ingest(
            Arc::clone(&ring),
            Arc::clone(&ring_cv),
            &cfg.scout_addr,
            Arc::clone(&cfg.active),
            cfg.scout_chunk_ms,
        )?;
        Ok(Self {
            mgr,
            ring,
            ring_cv,
            merge_gap_s: cfg.merge_gap_s,
            active: cfg.active,
            running: cfg.running,
            flush_paragraph: cfg.flush_paragraph,
            audio_store: Arc::new(AudioStore::new(DEFAULT_CAP_SAMPLES)),
            batch_asr,
        })
    }

    /// Use an already-loaded [`OnnxRuntimeManager`] (e.g. shared with another stage); spawns the
    /// ingest thread against `cfg.scout_addr`.
    pub fn new_with_mgr(mgr: Arc<OnnxRuntimeManager>, cfg: Stage1Config) -> Result<Self> {
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
        spawn_ingest(
            Arc::clone(&ring),
            Arc::clone(&ring_cv),
            &cfg.scout_addr,
            Arc::clone(&cfg.active),
            cfg.scout_chunk_ms,
        )?;
        Ok(Self {
            mgr,
            ring,
            ring_cv,
            merge_gap_s: cfg.merge_gap_s,
            active: cfg.active,
            running: cfg.running,
            flush_paragraph: cfg.flush_paragraph,
            audio_store: Arc::new(AudioStore::new(DEFAULT_CAP_SAMPLES)),
            batch_asr,
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
}

/// Spawn the scout→ring ingest thread (never blocks, never drops; reconnects on 2s backoff).
/// `active` controls whether it connects (see [`ScoutAudioSource::with_active`]); `chunk_ms`
/// (Some) asks scout to aggregate source buffers into ~N-ms HTTP chunks (`/audio?chunk_ms=N`).
fn spawn_ingest(
    ring: Arc<Mutex<AudioRing>>,
    ring_cv: Arc<Condvar>,
    scout_addr: &str,
    active: Arc<AtomicBool>,
    chunk_ms: Option<u64>,
) -> Result<()> {
    let src = ScoutAudioSource::with_active(scout_addr.to_string(), WINDOW, active)
        .with_chunk_ms(chunk_ms);
    thread::Builder::new()
        .name("aura-stage1-ingest".into())
        .spawn(move || {
            src.stream(
                move |win| {
                    let mut g = ring.lock().unwrap();
                    g.push(win);
                    drop(g);
                    // Wake the consume loop — it sleeps on the condvar between frames
                    // (deadline-driven, no polling).
                    ring_cv.notify_all();
                },
                Duration::from_secs(2),
            );
        })?;
    Ok(())
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

    /// Discard the in-progress sentence (neither pass produced text — noise). Clears `active`
    /// without recording anything.
    fn drop_active(&mut self) {
        if let Some(w) = self.open.as_mut() {
            w.active = false;
        }
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

/// Turn settled spans into a [`VadParagraph`] and emit `ParagraphEdge`: concat the clips from the
/// store (once — the paragraph keeps the `Arc`), re-run the batch model over the concatenated
/// PCM (the authoritative paragraph text), then evict the clips. An all-discarded paragraph (no
/// sentences) emits nothing and just vanishes.
fn emit_paragraph_edge(
    settled: SettledParagraph,
    store: &AudioStore,
    batch_asr: &dyn AsrProvider,
    sr: u32,
    on_event: &mut dyn FnMut(Stage1Event),
) {
    if settled.sentences.is_empty() {
        return;
    }
    let ids: Vec<AudioId> = settled.sentences.iter().map(|s| s.audio_id).collect();
    let pcm = Arc::new(store.concat(&ids));
    // ★单句段落免重跑:段落 batch 的意义是"跨段上下文重新整听"——只有一个段时拼接
    // PCM 与该句 PCM 完全相同,句级 batch 刚刚跑过同一音频,直接复用其结果(含 None:
    // 远程失败后立刻重试大概率仍失败,徒增 settle 延迟)。单句是常态(merge 仅发生在
    // <merge_gap 的停顿后),此优化省掉大多数段落的一整次 batch 调用。
    let (batch_text, asr_ms) = if settled.sentences.len() == 1 {
        debug!("单句段落——复用句级 batch 结果,跳过整段重跑");
        (settled.sentences[0].batch_text.clone(), 0u64)
    } else {
        // `asr_ms` 计时:本 commit 简单写 0(性能埋点 v1);后续可在此位置包
        // std::time::Instant::now() / elapsed() 替换为真实墙钟,值存到 VadParagraph.batch_asr_ms
        // 通过 paragraph 日志输出。
        let t0 = std::time::Instant::now();
        let text = batch_asr.recognize(&pcm, sr).ok().filter(|t| !t.trim().is_empty());
        (text, t0.elapsed().as_millis() as u64)
    };
    let streaming_text =
        settled.sentences.iter().map(|s| s.streaming_text.as_str()).collect::<String>();
    let start_s = settled.sentences.first().map(|s| s.start_s).unwrap_or(0.0);
    let end_s = settled.sentences.last().map(|s| s.end_s).unwrap_or(0.0);
    on_event(Stage1Event::ParagraphEdge {
        paragraph: VadParagraph {
            id: settled.paragraph_id,
            sentences: settled.sentences,
            start_s,
            end_s,
            streaming_text,
            batch_text,
            pcm,
            batch_asr_ms: asr_ms,
        },
    });
    // The paragraph's Arc PCM is now the only remaining copy — release the per-sentence clips.
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

    /// 定稿一个 VAD 句(EOS 臂):finalize 流式会话 → streaming_text,句 PCM 跑 batch,
    /// emit `Batch`(及可能的 `ParagraphEdge`)。`fallback_pcm` = 流式未配置时的 VAD
    /// edge-extended 句。返回 true = 句被丢弃(双路文本都空,噪声句)。
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
    ) -> bool {
        // 句 PCM = 流式 session 累积的完整音频(含句首 soft onset)——与流式听到的完全一致,
        // 区别只在 batch 一次整句听(大块)vs 流式逐帧听(小块)。流式未配置时 fallback VAD 句。
        let sentence_pcm = sess.as_ref().map(|a| a.pcm.clone()).unwrap_or(fallback_pcm);
        let streaming_text = match (self.mgr.streaming_asr(), sess.as_ref()) {
            (Some(asr), Some(a)) => asr.finalize_and_result(&a.stream),
            _ => String::new(),
        };
        // One batch pass over the sentence's PCM. Err (remote network) and empty text → None.
        // `asr_ms` 计时 batch 模型调用时长(纯 wall-clock,从进 recognize 到响应落盘),
        // 用于性能评估:对比不同 ASR 后端(sensevoice / qwen3-asr)/不同输入长度 / GPU vs CPU。
        let asr_t0 = std::time::Instant::now();
        let batch_text = self
            .batch_asr
            .recognize(&sentence_pcm, sr)
            .ok()
            .filter(|t| !t.trim().is_empty());
        let asr_ms = asr_t0.elapsed().as_millis() as u64;
        // Neither pass produced text → noise sentence: discard entirely.
        if streaming_text.trim().is_empty() && batch_text.is_none() {
            debug!("sentence discarded — neither streaming nor batch produced text");
            tracker.drop_active();
            return true;
        }
        // Speech onset back-derived from the PCM duration (SOS was retroactive, so its
        // wall-clock IS the EOS instant).
        let start_s = (end_s - sentence_pcm.len() as f64 / sr as f64).max(0.0);
        let sentence = VadSentence {
            id: *cur_sentence,
            audio_id: self.audio_store.insert(sentence_pcm),
            start_s,
            end_s,
            streaming_text,
            batch_text,
        };
        let (settled, paragraph_id, sentences) = tracker.on_eos(sentence);
        // A big gap settled the previous paragraph FIRST — emit it before this sentence's Batch.
        if let Some(s) = settled {
            emit_paragraph_edge(s, &self.audio_store, &*self.batch_asr, sr, on_event);
        }
        // 句级日志(debug):段落/段 id、音频时长、batch 模型调用耗时、两路文本、会话喂帧数。
        if let Some(s) = sentences.last() {
            debug!(
                paragraph_id = paragraph_id,
                sentence_id = s.id,
                time_ms = ((s.end_s - s.start_s) * 1000.0).round() as u64,
                fed,
                asr_ms,
                batch = s.batch_text.as_deref().unwrap_or("(none)"),
                streaming = %s.streaming_text,
                "句定稿"
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
        on_event(Stage1Event::Batch { paragraph_id, sentences });
        false
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
    // TODO(R5 残余): 轮询已除(2026-08-18 —— ring 挂 Condvar,无帧时挂起等 ingest notify,
    // 仅真实截止时间唤醒,空闲零唤醒);仍待整改:batch 调用还在消费线程内同步执行
    // (远程 ~3.5s/次会暂停流式),以及 run 仍占用整线程的阻塞模型。
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
            if self.flush_paragraph.load(Ordering::Acquire) {
                if !speaking {
                    match tracker.force_settle() {
                        Some(settled) => {
                            self.flush_paragraph.store(false, Ordering::Release);
                            info!(paragraph_id = settled.paragraph_id, sentences = settled.sentences.len(),
                                "flush: 主动归档(跳过 merge_gap 等待)");
                            emit_paragraph_edge(settled, &self.audio_store, &*self.batch_asr, sr, on_event);
                            sess = sasr.map(|asr| ActiveSession::new(asr.create_session())); // 段落边界重置会话
                        }
                        None if !tracker.has_open_paragraph() => {
                            self.flush_paragraph.store(false, Ordering::Release);
                        }
                        None => {} // 句进行中 → 挂起,等 EOS 后下一 tick 强制定稿
                    }
                }
            }
            if let Some(settled) = tracker.check_settle(now_s, speaking) {
                emit_paragraph_edge(settled, &self.audio_store, &*self.batch_asr, sr, on_event);
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
                        if self.finalize_sentence(
                            a, &mut tracker, &mut cur_sentence, sr, end_s, ev.pcm.clone(), fed, on_event,
                        ) {
                            continue; // 噪声句(双路文本都空)——丢弃
                        }
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

    /// Counting batch-ASR stub — proves the single-sentence paragraph skips the re-run.
    struct CountingAsr(std::sync::Mutex<usize>);
    impl AsrProvider for CountingAsr {
        fn recognize(&self, _pcm: &[i16], _sr: u32) -> anyhow::Result<String> {
            *self.0.lock().unwrap() += 1;
            Ok("段落重跑".into())
        }
    }

    fn sentence_into(store: &AudioStore, id: SentenceId, batch: Option<&str>) -> VadSentence {
        VadSentence {
            id,
            audio_id: store.insert(vec![1i16; 1600]),
            start_s: id as f64,
            end_s: id as f64 + 0.1,
            streaming_text: format!("流式{id}"),
            batch_text: batch.map(|b| b.to_string()),
        }
    }

    #[test]
    fn single_sentence_paragraph_reuses_sentence_batch_no_rerun() {
        let store = AudioStore::new(1_000_000);
        let asr = CountingAsr(std::sync::Mutex::new(0));
        let mut events = Vec::new();
        // batch Some → propagated verbatim; None → propagates as None (no retry either).
        for batch in [Some("句级结果"), None] {
            events.clear();
            let settled = SettledParagraph {
                paragraph_id: 1,
                sentences: vec![sentence_into(&store, 1, batch)],
            };
            emit_paragraph_edge(settled, &store, &asr, 16000, &mut |ev| events.push(ev));
            assert_eq!(*asr.0.lock().unwrap(), 0, "单句段落绝不重跑 batch");
            match &events[0] {
                Stage1Event::ParagraphEdge { paragraph } => assert_eq!(
                    paragraph.batch_text.as_deref(),
                    batch,
                    "段落 batch_text = 句级结果原样复用(含 None)"
                ),
                other => panic!("expected ParagraphEdge, got {other:?}"),
            }
        }
    }

    #[test]
    fn multi_sentence_paragraph_reruns_batch_once() {
        let store = AudioStore::new(1_000_000);
        let asr = CountingAsr(std::sync::Mutex::new(0));
        let settled = SettledParagraph {
            paragraph_id: 1,
            sentences: vec![sentence_into(&store, 1, Some("句1")), sentence_into(&store, 2, Some("句2"))],
        };
        let mut events = Vec::new();
        emit_paragraph_edge(settled, &store, &asr, 16000, &mut |ev| events.push(ev));
        assert_eq!(*asr.0.lock().unwrap(), 1, "多句段落恰好重跑一次");
        match &events[0] {
            Stage1Event::ParagraphEdge { paragraph } => {
                assert_eq!(paragraph.batch_text.as_deref(), Some("段落重跑"));
            }
            other => panic!("expected ParagraphEdge, got {other:?}"),
        }
    }

    #[test]
    fn drop_active_discards_without_recording() {
        let mut t = ParagraphTracker::new(2.5);
        let _ = t.on_sos(); // opens empty paragraph 0, allocates sentence 0, active=true
        t.drop_active(); // noise → active=false, paragraph 0 stays open but empty
        // Empty paragraph → settle timeout has nothing to close.
        assert!(t.check_settle(100.0, false).is_none(), "no sentences → nothing to settle");
        // The next sentence reuses the still-open paragraph.
        let s2 = t.on_sos();
        let (_, w, _) = t.on_eos(sentence(s2, 1.0, 1.1));
        assert_eq!(w, 1, "paragraph reused (not re-opened) after drop_active");
    }
}
