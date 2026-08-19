//! Stage1Recognizer — encapsulates the Stage1 "noodle": the audio ring + omni-scout ingest
//! thread + Silero VAD + per-segment streaming sessions + per-segment batch passes + the
//! window tracker. Owns ALL the loop state. It runs the consume loop internally and emits
//! [`Stage1Event`]s — it does NOT touch files or run Stage2 (that's `pipeline`'s job,
//! `audio_aura_core::Pipeline`).
//!
//! Boundary paradigm (docs/aura/stages.md): the VAD gap (`min_silence`) closes a
//! [`VadSegment`] (its own streaming session per D1 + one batch pass, packed as a `Batch`
//! event); the merge window (`merge_gap`) closes a [`VadWindow`] (concatenated PCM re-run
//! through the batch model, packed as a `WindowEdge` event). PCM lives in the
//! [`AudioStore`] by id — events carry ids + texts only, plus the window's shared
//! `Arc<Vec<i16>>` assembled once at settle.
//!
//! ```ignore
//! let exec = OnnxStage1Recognizer::new(Stage1Config::new(scout_addr))?;
//! exec.run(&mut |ev| match ev {
//!     Stage1Event::StreamFragment { window_id, segment_id, text, .. } => println!("…{text}"),
//!     Stage1Event::Batch { window_id, segments } => stage2.calibrate_window(window_id, &segments),
//!     Stage1Event::WindowEdge { window } => stage2.calibrate_final(&window),
//! });
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use tracing::{debug, warn};

use crate::audio_store::{AudioStore, DEFAULT_CAP_SAMPLES};
use crate::buffer::AudioRing;
use crate::scout::ScoutAudioSource;
use crate::{AudioId, SegmentId, Stage1Event, VadEventKind, VadSegment, VadWindow, WindowId};
// ONNX 语音栈在 dp-models(feature `speech`)——audio-aura 不再直接依赖 sherpa-onnx。
use dp_models::onnx::{
    AsrBackend, AsrConfig, OnnxRuntimeManager, StreamingAsrConfig, StreamingSession, VadConfig,
    WINDOW,
};
use dp_models::{http::HttpAsr, AsrProvider, ProviderKind};

/// Default ring capacity: 10 min @ 16 kHz mono (~19 MB).
const DEFAULT_RING_CAP: usize = 16_000 * 600;
/// Streaming-partial decode cadence: every N windows (~0.5s @ 32ms Silero windows).
const PARTIAL_EVERY_FRAMES: u32 = 15;
/// Stale-session watchdog: reset the streaming session when its partial has been UNCHANGED
/// this long AND no EOS came — that means VAD never latched (audio below `threshold` =
/// discard-by-design), and its residue (hallucinated repetitions included) must NOT leak
/// into whatever segment closes next (2026-08-17 实测:35s 悬置会话把上一段幻觉文本卷进
/// 下一句). Real speech never trips this: a ≥min_silence pause closes the segment via EOS,
/// which resets the session long before the partial could go stale.
const STALE_SESSION_RESET: Duration = Duration::from_secs(8);

/// 能量预门 RMS 门限(i16 幅度)。帧能量低于此值 **且** 无流式 partial(空闲)→ 判为"安静",
/// 跳过 Silero VAD 推理与流式解码(NN 贵,静音期占绝大多数时间)。仍喂 accept_waveform +
/// 累积 PCM(纯缓冲,便宜——保 D1 连续喂帧与流式/batch 共享音频)。语音 RMS 通常 500+,
/// 软起音 ~200-500,数字静音 ~0。200 为保守门限:只跳真正安静的帧,不截软起音。
const VAD_GATE_RMS: f32 = 200.0;

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
    /// ★Merge-window gap (seconds) — the UPPER bound of the medium-interval window. VAD fires
    /// EOS on every pause ≥ `min_silence` (kept low, ~1.0s, so each segment's batch pass kicks
    /// in fast); a following segment joins the SAME window when the inter-speech silence <
    /// this. Only a gap ≥ this (or no new speech for this long) closes the window →
    /// `WindowEdge`. The lower bound is implicit: `min_silence` is what splits segments in the
    /// first place, so the effective window is (min_silence, merge_gap) ≈ 1–2.5s. Decouples
    /// "VAD sensitivity" from "what's one utterance". 0 → every segment is its own window.
    pub merge_gap_s: f64,
    /// Batch-ASR switch (config `asr.backend: disable`): false → the batch model is NOT
    /// loaded and every batch pass returns empty (`batch_text` stays `None` — the legal
    /// "batch unavailable" state; consumers fall back to streaming text by design).
    /// Streaming + VAD unaffected. Defaults to true.
    pub batch_enabled: bool,
    /// Shared connection toggle (see [`ScoutAudioSource::with_active`]). Flip to false to stop
    /// ingesting from scout (does NOT kill scout). Defaults to true.
    pub active: Arc<AtomicBool>,
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
    /// sherpa. Streaming ASR + VAD stay local sherpa (real-time partials need low latency).
    pub fn with_remote_asr(mut self, endpoint: impl Into<String>) -> Self {
        self.asr_kind = ProviderKind::Remote { endpoint: endpoint.into() };
        self
    }
}

/// A Stage1 recognizer: audio in → [`Stage1Event`]s out. `run` blocks forever (drives the
/// ingest+consume loop) and invokes `on_event` for each interim partial / settled segment /
/// closed window.
pub trait Stage1Recognizer {
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) -> !;
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
    /// Merge-window gap (s) — see [`Stage1Config::merge_gap_s`].
    merge_gap_s: f64,
    /// VAD 尾静音定段时长 (s) — 能量门冷却期的依据(见 [`Stage1Config::vad`])。
    min_silence_s: f32,
    active: Arc<AtomicBool>,
    /// The PCM store: segments' clips live here by id until their window settles.
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
            (ProviderKind::Remote { endpoint }, _) => Arc::new(HttpAsr::new(endpoint.clone())),
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
            min_silence_s: cfg.vad.min_silence_duration,
            active: cfg.active,
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
            (ProviderKind::Remote { endpoint }, _) => Arc::new(HttpAsr::new(endpoint.clone())),
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
            min_silence_s: cfg.vad.min_silence_duration,
            active: cfg.active,
            audio_store: Arc::new(AudioStore::new(DEFAULT_CAP_SAMPLES)),
            batch_asr,
        })
    }

    /// Access the underlying ONNX model manager (e.g. for diagnostics / direct ASR calls).
    pub fn manager(&self) -> &Arc<OnnxRuntimeManager> {
        &self.mgr
    }

    /// The PCM store this recognizer owns — clips are addressable by [`AudioId`] until their
    /// window settles (then evicted; the window's `Arc<Vec<i16>>` is the surviving copy).
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

/// Block until a full Silero window is available in the ring (wakes on the ingest thread's
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

// ── Window tracker: pure windowing decisions over wall-clock SOS/EOS (unit-testable, no I/O) ──
// The recognizer owns the ASR side (sessions, batch passes, the AudioStore); this tracker owns
// ONLY the boundary math — which segment belongs to which window, and when a window closes.

/// The open window: its settled segments + whether a segment is in progress (SOS seen,
/// EOS pending). The in-progress segment's id/timing live recognizer-side ([`ActiveSession`]);
/// the tracker only needs "is one active" for settle suppression.
struct OpenWindow {
    window_id: WindowId,
    segments: Vec<VadSegment>,
    active: bool,
}

/// A window closed by a big gap or the settle-timeout — the recognizer turns this into a
/// [`VadWindow`] (concat PCM + window-level batch re-run) and emits `WindowEdge`.
struct SettledSpans {
    window_id: WindowId,
    segments: Vec<VadSegment>,
}

struct WindowTracker {
    merge_gap_s: f64,
    next_seg_id: SegmentId,
    next_win_id: WindowId,
    open: Option<OpenWindow>,
}

impl WindowTracker {
    fn new(merge_gap_s: f64) -> Self {
        Self { merge_gap_s, next_seg_id: 1, next_win_id: 1, open: None }
    }

    /// VAD StartOfSpeech. NOTE: the SOS is RETROACTIVE — it fires at the segment's EOS instant
    /// (its wall-clock IS the EOS time, NOT the speech onset), so the merge/split decision
    /// CANNOT happen here (using the EOS instant as the onset would inflate every gap by the
    /// segment's own duration and settle on EVERY segment — the "window never has >1 segment"
    /// bug). This only allocates the segment id + marks the window active; the settle decision
    /// moves to [`Self::on_eos`], which back-derives the true speech onset from the PCM.
    fn on_sos(&mut self) -> SegmentId {
        if self.open.is_none() {
            let id = self.next_win_id;
            self.next_win_id += 1;
            self.open = Some(OpenWindow { window_id: id, segments: Vec::new(), active: false });
        }
        let segment_id = self.next_seg_id;
        self.next_seg_id += 1;
        self.open.as_mut().expect("window just ensured").active = true;
        segment_id
    }

    /// Settle the open window iff the gap from `onset` (the NEXT segment's true speech start)
    /// back to its last segment ≥ merge_gap. `onset` must be the back-derived start, not the
    /// retroactive SOS instant.
    fn settle_if_gap(&mut self, onset: f64) -> Option<SettledSpans> {
        let gap = {
            let w = self.open.as_ref()?;
            let last = w.segments.last()?;
            onset - last.end_s
        };
        if gap >= self.merge_gap_s {
            self.take_open()
        } else {
            None
        }
    }

    /// Record a completed segment. Settles the open window FIRST when the gap since its last
    /// segment ≥ merge_gap (using `seg.start_s`, the BACK-DERIVED true onset), then pushes this
    /// segment into the (possibly fresh) window. Returns (settled spans, window id, ALL segments
    /// so far) — the payload IS the window, so Stage2 stays stateless.
    fn on_eos(&mut self, seg: VadSegment) -> (Option<SettledSpans>, WindowId, Vec<VadSegment>) {
        let settled = self.settle_if_gap(seg.start_s);
        if self.open.is_none() {
            // First segment, or the previous window just settled.
            let id = self.next_win_id;
            self.next_win_id += 1;
            self.open = Some(OpenWindow { window_id: id, segments: Vec::new(), active: false });
        }
        let w = self.open.as_mut().expect("window just ensured");
        w.active = false;
        w.segments.push(seg);
        (settled, w.window_id, w.segments.clone())
    }

    /// Discard the in-progress segment (neither pass produced text — noise). Clears `active`
    /// without recording anything.
    fn drop_active(&mut self) {
        if let Some(w) = self.open.as_mut() {
            w.active = false;
        }
    }

    /// Settle-timeout probe (call every loop tick with the current wall-clock). Closes the
    /// window when it has been silent (no active speech) for ≥ `merge_gap_s` — this is how the
    /// TRAILING window finalizes. Suppressed while a segment is in progress AND while `speaking`
    /// is true — the streaming session still has a non-empty partial, i.e. someone is talking
    /// right now but this VAD's SOS for that speech hasn't arrived yet (it's RETROACTIVE, comes
    /// with EOS). Without this suppression the wall-clock timeout would fire mid-sentence and
    /// split the next segment into a fresh window — the "window never has >1 segment" bug.
    fn check_settle(&mut self, now: f64, speaking: bool) -> Option<SettledSpans> {
        let w = self.open.as_ref()?;
        if w.active || speaking {
            return None;
        }
        let last = w.segments.last()?;
        if now - last.end_s >= self.merge_gap_s {
            self.take_open()
        } else {
            None
        }
    }

    /// Seconds until [`Self::check_settle`] would close the open window (None = no pending
    /// settle: nothing open, no segments yet, a segment in progress, or `speaking` — the next
    /// segment's speech is ongoing but its SOS hasn't arrived yet). Drives the consume loop's
    /// condvar deadline — wake exactly when the trailing window is due, not on a poll cadence.
    fn settle_deadline(&self, now: f64, speaking: bool) -> Option<f64> {
        let w = self.open.as_ref()?;
        if w.active || speaking {
            return None;
        }
        let last = w.segments.last()?;
        Some((self.merge_gap_s - (now - last.end_s)).max(0.0))
    }

    fn take_open(&mut self) -> Option<SettledSpans> {
        self.open.take().map(|w| SettledSpans { window_id: w.window_id, segments: w.segments })
    }

    /// The ids the segment currently being spoken WILL get: the open window's id (or the next
    /// one when nothing is open) + the next segment id. Used to key live `StreamFragment`
    /// partials —
    /// this VAD emits SOS RETROACTIVELY (with EOS), so the real assignment only exists at EOS.
    /// Authoritative grouping arrives with the `Batch`/`WindowEdge` events.
    fn prospective(&self) -> (WindowId, SegmentId) {
        let w = self.open.as_ref().map(|w| w.window_id).unwrap_or(self.next_win_id);
        (w, self.next_seg_id)
    }
}

/// Turn settled spans into a [`VadWindow`] and emit `WindowEdge`: concat the clips from the
/// store (once — the window keeps the `Arc`), re-run the batch model over the concatenated
/// PCM (the authoritative window text), then evict the clips. An all-discarded window (no
/// segments) emits nothing and just vanishes.
fn emit_window_edge(
    settled: SettledSpans,
    store: &AudioStore,
    batch_asr: &dyn AsrProvider,
    sr: u32,
    on_event: &mut dyn FnMut(Stage1Event),
) {
    if settled.segments.is_empty() {
        return;
    }
    let ids: Vec<AudioId> = settled.segments.iter().map(|s| s.audio_id).collect();
    let pcm = Arc::new(store.concat(&ids));
    // ★单段窗口免重跑:窗口 batch 的意义是"跨段上下文重新整听"——只有一个段时拼接
    // PCM 与该段 PCM 完全相同,段级 batch 刚刚跑过同一音频,直接复用其结果(含 None:
    // 远程失败后立刻重试大概率仍失败,徒增 settle 延迟)。单段是常态(merge 仅发生在
    // <merge_gap 的停顿后),此优化省掉大多数窗口的一整次 batch 调用。
    let batch_text = if settled.segments.len() == 1 {
        debug!("单段窗口——复用段级 batch 结果,跳过整窗重跑");
        settled.segments[0].batch_text.clone()
    } else {
        batch_asr.recognize(&pcm, sr).ok().filter(|t| !t.trim().is_empty())
    };
    let streaming_text =
        settled.segments.iter().map(|s| s.streaming_text.as_str()).collect::<String>();
    let start_s = settled.segments.first().map(|s| s.start_s).unwrap_or(0.0);
    let end_s = settled.segments.last().map(|s| s.end_s).unwrap_or(0.0);
    on_event(Stage1Event::WindowEdge {
        window: VadWindow {
            id: settled.window_id,
            segments: settled.segments,
            start_s,
            end_s,
            streaming_text,
            batch_text,
            pcm,
        },
    });
    // The window's Arc PCM is now the only remaining copy — release the per-segment clips.
    store.evict(&ids);
}

/// The live streaming session + its partial-throttle state. D1 adaptation: sherpa's VAD
/// emits SOS RETROACTIVELY (together with EOS — the segment only pops complete), so the
/// session CANNOT be created at speech onset. Instead it is fed CONTINUOUSLY and RESET at
/// every segment boundary (EOS) and window settle — each session therefore covers exactly
/// [previous boundary, this EOS] ≈ this one segment (+ surrounding silence, which decodes
/// to nothing). Per-segment attribution is preserved; live partials keep flowing.
struct ActiveSession {
    stream: StreamingSession,
    frames_since_partial: u32,
    last_partial: String,
    /// When `last_partial` last CHANGED (decayed text ⇒ stale ⇒ watchdog reset).
    last_change: Instant,
    /// Diagnostic: frames fed since the last reset.
    fed: u32,
    /// Every fed frame, accumulated — the EXACT audio this streaming session heard. At EOS this
    /// becomes the segment's PCM (shared with the batch ASR), so streaming and batch see the
    /// same audio — including the soft onset BEFORE VAD's threshold crossing, which the VAD's
    /// own segment cuts off (the "batch drops the first 2-3 chars" bug). Bounded by the segment
    /// length (+ boundary silence), reset at every EOS / window settle.
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

impl Stage1Recognizer for OnnxStage1Recognizer {
    // TODO(R5 残余): 轮询已除(2026-08-18 —— ring 挂 Condvar,无帧时挂起等 ingest notify,
    // 仅真实截止时间唤醒,空闲零唤醒);仍待整改:batch 调用还在消费线程内同步执行
    // (远程 ~3.5s/次会暂停流式),以及 run 仍占用整线程的阻塞模型。
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) -> ! {
        let sr = 16000u32;
        let start = Instant::now();
        let mut last_diag = Instant::now();
        let mut frames_in = 0u64;

        let sasr = self.mgr.streaming_asr();
        // Continuously-fed streaming session (reset at every segment/window boundary — see
        // [`ActiveSession`]). `cur_seg`/`cur_win` carry the retroactive SOS's ids to the EOS
        // arm within the SAME push_frame batch (SOS+EOS always arrive together).
        let mut sess: Option<ActiveSession> = sasr.map(|asr| ActiveSession::new(asr.create_session()));
        let mut ring_empty_since: Option<Instant> = None;
        let mut tracker = WindowTracker::new(self.merge_gap_s);
        let mut cur_seg: SegmentId = 0;
        // silence-feed 的节流计时器:喂送静音逼 VAD EOS 时,每 100ms 至多喂一帧(避免 CPU 空转)。
        let mut last_silence_feed = Instant::now();
        // 能量门冷却:说话(帧 RMS ≥ 门限)后的这段时间内不跳过 VAD——VAD 需要累计尾静音
        // (min_silence)才能判 EOS。冷却 = min_silence + 0.5s 余量。
        let speech_cooldown = Duration::from_secs_f64(self.min_silence_s as f64 + 0.5);
        // 上次"有能量"的时刻。初始为冷却已过 → 空闲立即进入能量门。
        let mut last_loud = Instant::now() - speech_cooldown;

        loop {
            // Connection toggle: when the scout connection is paused, skip VAD/ASR (the ingest
            // thread also stops feeding the ring, so it drains to empty shortly). Park on the
            // condvar — the next pushed frame (or the resumed connection's first chunk) wakes
            // us to re-check the toggle. No timer.
            if !self.active.load(Ordering::Relaxed) {
                let _ = wait_frame(&self.ring, &self.ring_cv, WINDOW, None);
                continue;
            }
            // Settle the trailing window: no follow-up segment came for merge_gap — it's done.
            // (Suppressed while speech is in progress inside the tracker.) The wait is
            // unavoidable — you must observe the gap to know it ended — but the per-segment
            // Batch results have been showing live text throughout, so it doesn't lag.
            let now_s = start.elapsed().as_secs_f64();
            // 回溯式 VAD:下一段的 SOS 要等它 EOS 才到。此刻若流式 session 仍在产出
            // partial(有人正在说话),绝不能按墙钟超时定稿——否则下一段(其 SOS 尚未到达)
            // 会被错划进新窗口,导致窗口永远只有 1 个 segment。真正的 gap 判断由下一段
            // EOS 到达时的 settle_if_gap 完成。
            let speaking = sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
            if let Some(settled) = tracker.check_settle(now_s, speaking) {
                emit_window_edge(settled, &self.audio_store, &*self.batch_asr, sr, on_event);
                // Window boundary ⇒ fresh streaming session (don't bleed encoder context
                // across windows).
                sess = sasr.map(|asr| ActiveSession::new(asr.create_session()));
            }
            // Stale-session watchdog: VAD-unlatched audio is discard-by-design — its residue
            // must not linger in the session until the next (loud) utterance swallows it.
            // See [`STALE_SESSION_RESET`].
            if let Some(a) = sess.as_ref() {
                if !a.last_partial.is_empty() && a.last_change.elapsed() >= STALE_SESSION_RESET {
                    warn!(
                        stale_s = a.last_change.elapsed().as_secs(),
                        partial = %a.last_partial,
                        "流式会话停滞重置——VAD 未定段的微弱音频不残留到下一句"
                    );
                    sess = sasr.map(|asr| ActiveSession::new(asr.create_session()));
                }
            }
            if last_diag.elapsed() >= Duration::from_secs(3) {
                let rlen = self.ring.lock().unwrap().len();
                // NOTE: `is_speaking()` is useless with this retroactive VAD (only true for
                // the instant a segment pops) — the meaningful live signal is the partial.
                let has_partial = sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
                debug!(frames = frames_in, ring = rlen, has_partial, "stage1 diag");
                last_diag = Instant::now();
            }

            // Next wake deadline: the earliest REAL timer, or None = nothing time-based is
            // pending → park indefinitely (wake only on incoming audio). This is what
            // replaces polling — no heartbeat, no idle wakeups.
            let mut wake_at: Option<Duration> = None;
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

            // drain one Silero window (512 samples = 32ms) when available
            let frame = {
                let mut g = self.ring.lock().unwrap();
                if g.has_frame(WINDOW) {
                    ring_empty_since = None;
                    g.drain(WINDOW)
                } else {
                    drop(g);
                    // Ring empty: if > 2s AND the active segment has partials (were speaking),
                    // feed silence to VAD so it fires EOS naturally — prevents the scenario
                    // where audio source drops mid-utterance and VAD never evaluates silence.
                    ring_empty_since.get_or_insert_with(Instant::now);
                    let since = ring_empty_since.unwrap();
                    let has_partial =
                        sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
                    if since.elapsed() > Duration::from_secs(2) && has_partial {
                        // Feed synthetic silence so VAD fires EOS when the source dropped
                        // mid-utterance — but at a 100ms cadence, NOT a CPU burn: each feed is
                        // one 32ms silence window, so ~1s of silence accumulates over ~3s wall.
                        if last_silence_feed.elapsed() >= Duration::from_millis(100) {
                            last_silence_feed = Instant::now();
                            debug!("ring empty > 2s during speech — feeding silence to force VAD EOS");
                            vec![0i16; WINDOW] // synthetic silence frame → VAD will fire EOS
                        } else {
                            // Park until the next feed tick (a real frame still wakes us early).
                            match wait_frame(&self.ring, &self.ring_cv, WINDOW, Some(Duration::from_millis(100))) {
                                Some(f) => {
                                    ring_empty_since = None;
                                    f
                                }
                                None => continue, // not a feed tick yet — re-run checks
                            }
                        }
                    } else {
                        // Park until the ingest thread pushes (condvar notify) or the next
                        // deadline above fires — 无睡眠轮询,空闲时零唤醒.
                        match wait_frame(&self.ring, &self.ring_cv, WINDOW, wake_at) {
                            Some(f) => {
                                ring_empty_since = None;
                                f
                            }
                            None => continue, // deadline fired — re-run settle/watchdog checks
                        }
                    }
                }
            };
            frames_in += 1;

            // 能量预门:帧能量低于门限 **且** 距上次有能量已过冷却期(真正空闲)→ 跳过 Silero
            // VAD 推理与流式解码(NN 贵)。仍喂 accept_waveform + 累积 PCM(便宜,保 D1 连续喂帧
            // 与共享音频)。
            // 不用 `speaking`(流式 partial)作条件——x-asr 在静音上幻觉复读会让 partial 恒非空,
            // 门永远不触发(自锁)。冷却期保证:说话中/说完尾静音(≤min_silence+0.5s)必喂 VAD
            // 才能累计静音判 EOS;只有静音持续过冷却期才判为真空闲。
            let frame_rms = crate::vad::rms(&frame);
            if frame_rms >= VAD_GATE_RMS {
                last_loud = Instant::now();
            }
            let idle_silence = frame_rms < VAD_GATE_RMS && last_loud.elapsed() > speech_cooldown;

            // (1) live streaming partial — the session is fed CONTINUOUSLY (D1 adaptation:
            //     this VAD's SOS is retroactive, so gating on speech-start is impossible);
            //     see [`ActiveSession`]. Throttle to ~0.5s, only on change. Keyed by the
            //     tracker's prospective ids (authoritative grouping comes with Batch).
            //     NOT a Stage2 input (D2).
            if let (Some(asr), Some(a)) = (sasr, sess.as_mut()) {
                // 空闲静音:accept_waveform + PCM 累积都跳过(纯缓冲)。
                //  · 会话在段边界 reset,静音缓冲对最终文本无贡献
                //  · pcm 不累积空闲静音 → 防止挂机时无限增长(否则 1h ≈ 100MB+);
                //    共享 PCM 的 soft-onset 修复在语音内部,不受影响
                //  · 说话中/尾静音(冷却期内)照常喂,保 D1 连续喂帧 + 共享音频
                if !idle_silence {
                    a.stream.accept_waveform(sr as i32, &frame);
                    a.pcm.extend_from_slice(&frame); // 流式与 batch 共用同一段音频
                }
                a.fed += 1;
                a.frames_since_partial += 1;
                // 空闲静音:跳过解码 NN(静音产出空文本);`frames_since_partial` 继续累计,
                // 能量一恢复就立刻解码(不丢音频——accept_waveform 一直在缓冲)。
                if a.frames_since_partial >= PARTIAL_EVERY_FRAMES && !idle_silence {
                    let partial = asr.decode_and_result(&a.stream);
                    if !partial.is_empty() && partial != a.last_partial {
                        let (window_id, segment_id) = tracker.prospective();
                        on_event(Stage1Event::StreamFragment {
                            window_id,
                            segment_id,
                            text: partial.clone(),
                            at_s: start.elapsed().as_secs_f64(),
                        });
                        a.last_partial = partial;
                        a.last_change = Instant::now();
                    }
                    a.frames_since_partial = 0;
                }
            }

            // (2) VAD boundaries → WindowTracker → Batch (per segment) / WindowEdge (settle).
            //     NOTE: this VAD emits SOS RETROACTIVELY — SOS+EOS arrive together in one
            //     push_frame batch when the finished segment pops. The SOS arm records the
            //     ids (for the segment the EOS arm in the same batch builds); the streaming
            //     session lifecycle is boundary-driven instead (see [`ActiveSession`]).
            //     空闲静音帧跳过 Silero(对"非说话"态等价于喂静音,省每帧 NN 推理);
            //     说话中/软起音(能量≥门限)照常喂,Silero 照常判起音/定段。
            if !idle_silence {
            for ev in self.mgr.vad().unwrap().push_frame(&frame) {
                match ev.kind {
                    VadEventKind::StartOfSpeech => {
                        // 回溯式 SOS:只分配段号 + 标记 active。merge/split 决策在 EOS
                        // 臂(那里能回推真实语音起点)——见 on_eos。
                        cur_seg = tracker.on_sos();
                    }
                    VadEventKind::EndOfSpeech => {
                        let end_s = start.elapsed().as_secs_f64();
                        // Finalize the CURRENT streaming session → this segment's
                        // streaming_text, then reset it (the next session covers exactly the
                        // next segment). `fed` is diagnostic.
                        let a = sess.take();
                        let fed = a.as_ref().map(|a| a.fed).unwrap_or(0);
                        let streaming_text = match (sasr, a.as_ref()) {
                            (Some(asr), Some(a)) => asr.finalize_and_result(&a.stream),
                            _ => String::new(),
                        };
                        sess = sasr.map(|asr| ActiveSession::new(asr.create_session()));
                        // 段 PCM = 流式 session 累积的完整音频(含段首 soft onset)——与流式
                        // 听到的完全一致,区别只在 batch 一次整段听(大块)vs 流式逐帧听(小块)。
                        // 流式未配置(sess 为 None)时 fallback VAD 的 edge-extended 段。
                        let seg_pcm = a.map(|a| a.pcm).unwrap_or_else(|| ev.pcm.clone());
                        // One batch pass over the segment's PCM. Err (remote network) and
                        // empty text both map to None.
                        let batch_text = self
                            .batch_asr
                            .recognize(&seg_pcm, sr)
                            .ok()
                            .filter(|t| !t.trim().is_empty());
                        // Neither pass produced text → noise segment: discard entirely.
                        if streaming_text.trim().is_empty() && batch_text.is_none() {
                            debug!("segment discarded — neither streaming nor batch produced text");
                            tracker.drop_active();
                            continue;
                        }
                        // Speech onset back-derived from the PCM duration (SOS was
                        // retroactive, so its wall-clock IS the EOS instant).
                        let start_s = (end_s - seg_pcm.len() as f64 / sr as f64).max(0.0);
                        let seg = VadSegment {
                            id: cur_seg,
                            audio_id: self.audio_store.insert(seg_pcm),
                            start_s,
                            end_s,
                            streaming_text,
                            batch_text,
                        };
                        let (settled, window_id, segments) = tracker.on_eos(seg);
                        // A big gap settled the previous window FIRST — emit it before this
                        // segment's Batch (its authoritative grouping).
                        if let Some(s) = settled {
                            emit_window_edge(s, &self.audio_store, &*self.batch_asr, sr, on_event);
                        }
                        // 段级日志(debug):每个 VadSegment 定稿时打印——窗口/段 id、
                        // 时长、两路文本(batch 失败显式标出)、会话喂帧数(诊断)。
                        // 刚定稿的段就是 segments 的最后一个(字符串已 move,从这借)。
                        if let Some(s) = segments.last() {
                            debug!(
                                window = window_id,
                                segment = s.id,
                                dur_ms = ((s.end_s - s.start_s) * 1000.0).round() as u64,
                                fed,
                                batch = s.batch_text.as_deref().unwrap_or("(none)"),
                                streaming = %s.streaming_text,
                                "段定稿(VadSegment)"
                            );
                        }
                        // Final stream fragment: the segment's DEFINITIVE streaming text
                        // (live partials only decode up to the last throttle frame; finalize
                        // is authoritative). Real ids now assigned by on_eos. Skipped when
                        // streaming produced nothing (batch-only segment).
                        if let Some(s) = segments.last().filter(|s| !s.streaming_text.is_empty()) {
                            on_event(Stage1Event::StreamFragment {
                                window_id,
                                segment_id: s.id,
                                text: s.streaming_text.clone(),
                                at_s: end_s,
                            });
                        }
                        on_event(Stage1Event::Batch { window_id, segments });
                    }
                }
            }
            } // !idle_silence — 空闲静音帧跳过 Silero VAD
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: SegmentId, start_s: f64, end_s: f64) -> VadSegment {
        VadSegment {
            id,
            audio_id: id,
            start_s,
            end_s,
            streaming_text: format!("s{id}"),
            batch_text: Some(format!("b{id}")),
        }
    }

    #[test]
    fn short_gap_absorbs_into_same_window() {
        let mut t = WindowTracker::new(2.5);
        let s1 = t.on_sos();
        let (settled, w1, segs) = t.on_eos(seg(s1, 0.0, 0.5));
        assert!(settled.is_none());
        assert_eq!(segs.len(), 1);

        // gap 1.0−0.5 = 0.5 < 2.5 → same window, second segment (merge happens at EOS,
        // where the true onset is back-derived).
        let s2 = t.on_sos();
        let (settled, w, segs) = t.on_eos(seg(s2, 1.0, 1.5));
        assert!(settled.is_none(), "short gap must NOT settle");
        assert_eq!(w, w1, "same window continues");
        assert_eq!(segs.len(), 2, "both segments in one window");
    }

    #[test]
    fn big_gap_settles_previous_window_and_opens_new_one() {
        let mut t = WindowTracker::new(2.5);
        let s1 = t.on_sos();
        let (_, w1, _) = t.on_eos(seg(s1, 0.0, 0.5));
        // gap 5.0−0.5 = 4.5 ≥ 2.5 → settle w1 at the next segment's EOS, open w2.
        let s2 = t.on_sos();
        let (settled, w2, segs) = t.on_eos(seg(s2, 5.0, 5.5));
        let s = settled.expect("big gap settles the previous window");
        assert_eq!(s.window_id, w1);
        assert_eq!(s.segments.len(), 1);
        assert_ne!(w2, w1, "a fresh window opens");
        assert!(w2 > w1, "window ids are monotonic");
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn settle_timeout_closes_trailing_window() {
        let mut t = WindowTracker::new(2.5);
        let s1 = t.on_sos();
        let (_, w1, _) = t.on_eos(seg(s1, 0.0, 0.5));
        assert!(t.check_settle(2.0, false).is_none(), "2.0 − 0.5 = 1.5 < 2.5, not yet");
        let s = t.check_settle(3.0, false).expect("3.0 − 0.5 = 2.5 ≥ merge_gap → settle");
        assert_eq!(s.window_id, w1);
        assert!(t.check_settle(10.0, false).is_none(), "nothing open anymore");
    }

    #[test]
    fn settle_deadline_counts_down_to_merge_gap() {
        // The condvar wake deadline: exactly when check_settle would fire (consumes loop
        // parks on the ring condvar instead of polling — this is its only wake source for
        // the trailing window).
        let mut t = WindowTracker::new(2.5);
        assert!(t.settle_deadline(0.0, false).is_none(), "nothing open yet");
        let s1 = t.on_sos();
        t.on_eos(seg(s1, 0.0, 0.5));
        assert!((t.settle_deadline(1.0, false).unwrap() - 2.0).abs() < 1e-9, "2.5 − (1.0 − 0.5)");
        assert!((t.settle_deadline(3.0, false).unwrap() - 0.0).abs() < 1e-9, "due now, clamped at 0");
        let _s2 = t.on_sos(); // segment in progress (active=true)
        assert!(t.settle_deadline(1.2, false).is_none(), "active segment ⇒ suppressed, no deadline");
    }

    #[test]
    fn active_segment_suppresses_settle_timeout() {
        // Regression guard: a long following segment must not be mistaken for "no
        // continuation" and force-split the window mid-speech.
        let mut t = WindowTracker::new(2.5);
        let s1 = t.on_sos();
        t.on_eos(seg(s1, 0.0, 0.5));
        let _s2 = t.on_sos(); // segment in progress (active=true)
        assert!(t.check_settle(100.0, false).is_none(), "active segment ⇒ settle suppressed");
    }

    #[test]
    fn speaking_suppresses_settle_waiting_for_retroactive_sos() {
        // 回溯式 VAD 的回归防护:下一段的 SOS 要等它的 EOS 才到——在它到达前,流式
        // session 的 partial 非空(=speaking=true)必须抑制 settle 超时。否则墙钟超时
        // 会在下一段说话时定稿,把它错划进新窗口(症状:窗口永远只有 1 个 segment)。
        let mut t = WindowTracker::new(2.5);
        let s1 = t.on_sos();
        t.on_eos(seg(s1, 0.0, 0.5));
        // 下一段正在说话(SOS 尚未到),墙钟已远超 merge_gap —— speaking=true 抑制。
        assert!(t.check_settle(100.0, true).is_none(), "speaking ⇒ settle suppressed");
        assert!(t.settle_deadline(100.0, true).is_none(), "speaking ⇒ no settle deadline");
        // 说话停止(speaking=false)后,同一时刻立刻能定稿。
        assert!(t.check_settle(100.0, false).is_some(), "not speaking ⇒ settle fires");
    }

    #[test]
    fn merge_gap_zero_makes_every_segment_its_own_window() {
        let mut t = WindowTracker::new(0.0);
        let s1 = t.on_sos();
        let (_, w1, _) = t.on_eos(seg(s1, 0.0, 0.5));
        // Any gap ≥ 0 settles at the next segment's EOS (gap 0.6 − 0.5 = 0.1 ≥ 0).
        let s2 = t.on_sos();
        let (settled, w2, _) = t.on_eos(seg(s2, 0.6, 0.7));
        assert_eq!(settled.expect("gap 0.1 ≥ 0 settles").window_id, w1);
        assert_ne!(w2, w1);
        // …and the settle timeout fires immediately after an EOS too.
        let s3 = t.on_sos();
        t.on_eos(seg(s3, 10.0, 10.5));
        assert!(t.check_settle(10.5, false).is_some(), "now − end = 0 ≥ 0 → settle");
    }

    /// Counting batch-ASR stub — proves the single-segment window skips the re-run.
    struct CountingAsr(std::sync::Mutex<usize>);
    impl AsrProvider for CountingAsr {
        fn recognize(&self, _pcm: &[i16], _sr: u32) -> anyhow::Result<String> {
            *self.0.lock().unwrap() += 1;
            Ok("窗口重跑".into())
        }
    }

    fn seg_into(store: &AudioStore, id: SegmentId, batch: Option<&str>) -> VadSegment {
        VadSegment {
            id,
            audio_id: store.insert(vec![1i16; 1600]),
            start_s: id as f64,
            end_s: id as f64 + 0.1,
            streaming_text: format!("流式{id}"),
            batch_text: batch.map(|b| b.to_string()),
        }
    }

    #[test]
    fn single_segment_window_reuses_segment_batch_no_rerun() {
        let store = AudioStore::new(1_000_000);
        let asr = CountingAsr(std::sync::Mutex::new(0));
        let mut events = Vec::new();
        // batch Some → propagated verbatim; None → propagates as None (no retry either).
        for batch in [Some("段级结果"), None] {
            events.clear();
            let settled = SettledSpans {
                window_id: 1,
                segments: vec![seg_into(&store, 1, batch)],
            };
            emit_window_edge(settled, &store, &asr, 16000, &mut |ev| events.push(ev));
            assert_eq!(*asr.0.lock().unwrap(), 0, "单段窗口绝不重跑 batch");
            match &events[0] {
                Stage1Event::WindowEdge { window } => assert_eq!(
                    window.batch_text.as_deref(),
                    batch,
                    "窗口 batch_text = 段级结果原样复用(含 None)"
                ),
                other => panic!("expected WindowEdge, got {other:?}"),
            }
        }
    }

    #[test]
    fn multi_segment_window_reruns_batch_once() {
        let store = AudioStore::new(1_000_000);
        let asr = CountingAsr(std::sync::Mutex::new(0));
        let settled = SettledSpans {
            window_id: 1,
            segments: vec![seg_into(&store, 1, Some("段1")), seg_into(&store, 2, Some("段2"))],
        };
        let mut events = Vec::new();
        emit_window_edge(settled, &store, &asr, 16000, &mut |ev| events.push(ev));
        assert_eq!(*asr.0.lock().unwrap(), 1, "多段窗口恰好重跑一次");
        match &events[0] {
            Stage1Event::WindowEdge { window } => {
                assert_eq!(window.batch_text.as_deref(), Some("窗口重跑"));
            }
            other => panic!("expected WindowEdge, got {other:?}"),
        }
    }

    #[test]
    fn drop_active_discards_without_recording() {
        let mut t = WindowTracker::new(2.5);
        let _ = t.on_sos(); // opens empty window 0, allocates seg 0, active=true
        t.drop_active(); // noise → active=false, window 0 stays open but empty
        // Empty window → settle timeout has nothing to close.
        assert!(t.check_settle(100.0, false).is_none(), "no segments → nothing to settle");
        // The next segment reuses the still-open window.
        let s2 = t.on_sos();
        let (_, w, _) = t.on_eos(seg(s2, 1.0, 1.1));
        assert_eq!(w, 1, "window reused (not re-opened) after drop_active");
    }
}
