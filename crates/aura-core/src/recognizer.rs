//! Stage1Recognizer — encapsulates the Stage1 "noodle": the audio ring + omni-scout ingest
//! thread + Silero VAD + per-segment streaming sessions + per-segment batch passes + the
//! window tracker. Owns ALL the loop state. It runs the consume loop internally and emits
//! [`Stage1Event`]s — it does NOT touch files or run Stage2 (that's `pipeline`'s job,
//! `audio_aura_core::Pipeline`).
//!
//! Boundary paradigm (docs/aura/vad-segment-model.md): the VAD gap (`min_silence`) closes a
//! [`VadSegment`] (its own streaming session per D1 + one batch pass, packed as a `Batch`
//! event); the merge window (`merge_gap`) closes a [`VadWindow`] (concatenated PCM re-run
//! through the batch model, packed as a `WindowEdge` event). PCM lives in the
//! [`AudioStore`] by id — events carry ids + texts only, plus the window's shared
//! `Arc<Vec<i16>>` assembled once at settle.
//!
//! ```ignore
//! let exec = OnnxStage1Recognizer::new(Stage1Config::new(scout_addr))?;
//! exec.run(&mut |ev| match ev {
//!     Stage1Event::Interim { window_id, segment_id, partial, .. } => println!("…{partial}"),
//!     Stage1Event::Batch { window_id, segments } => stage2.calibrate_window(window_id, &segments),
//!     Stage1Event::WindowEdge { window } => stage2.calibrate_final(&window),
//! });
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
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

/// Config for [`OnnxStage1Recognizer`] — paths + params for the VAD, batch ASR, and streaming ASR,
/// plus the omni-scout address, ring capacity, and the connection `active` flag.
#[derive(Clone)]
pub struct Stage1Config {
    pub scout_addr: String,
    pub vad: VadConfig,
    pub asr: AsrConfig,
    pub streaming: StreamingAsrConfig,
    pub ring_cap_samples: usize,
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
    /// Shared connection toggle (see [`ScoutAudioSource::with_active`]). Flip to false to stop
    /// ingesting from scout (does NOT kill scout). Defaults to true.
    pub active: Arc<AtomicBool>,
}

impl Stage1Config {
    /// Sensible defaults — model paths resolved via `shared` namespace `MODELS` (declared in
    /// this crate's `Cargo.toml` `[package.metadata.shared]`). Dev: `<workspace>/assets/models/`;
    /// prod: `~/.audio-aura/models/`. No `base` param needed — the caller never sees paths.
    pub fn new(scout_addr: impl Into<String>) -> Self {
        // TODO: 在一个 new 函数中使用了 IO 操作，会失败，将 IO 拆出去作为另一个函数
        let fs = shared::loader!();
        let p = |rel: &str| -> String {
            fs.resolve(rel)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        Self {
            scout_addr: scout_addr.into(),
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
            asr_kind: ProviderKind::Local,
            merge_gap_s: 5.0,
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Use Whisper (e.g. large-v3-turbo) as the batch ASR backend instead of SenseVoice.
    /// Model paths resolve via the same `MODELS` namespace.
    pub fn with_whisper_asr(mut self, language: &str) -> Self {
        let fs = shared::loader!();
        let p = |rel: &str| -> String {
            fs.resolve(rel).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
        };
        self.asr = AsrConfig {
            backend: AsrBackend::Whisper {
                encoder: p("MODELS::whisper/large-v3-turbo/encoder.onnx"),
                decoder: p("MODELS::whisper/large-v3-turbo/decoder.onnx"),
                language: language.into(),
            },
            tokens: p("MODELS::whisper/large-v3-turbo/tokens.txt"),
            ..Default::default()
        };
        self
    }

    /// Use Qwen3-Audio ASR (e.g. 1.7B int8) as the batch ASR backend instead of SenseVoice.
    /// `tokenizer` is a HF tokenizer DIRECTORY. Qwen3-ASR is autoregressive (encoder-decoder,
    /// LLM-style), so it is **slow on CPU** (sherpa-onnx ships CPU-only libs here) — useful as a
    /// high-accuracy offline backend, and fast once a CUDA build is available. `tokens` is left
    /// empty (Qwen3 loads its vocab from the tokenizer dir).
    pub fn with_qwen3_asr(mut self) -> Self {
        let fs = shared::loader!();
        let p = |rel: &str| -> String {
            fs.resolve(rel).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
        };
        self.asr = AsrConfig {
            backend: AsrBackend::Qwen3Asr {
                conv_frontend: p("MODELS::qwen3-asr/conv_frontend.onnx"),
                encoder: p("MODELS::qwen3-asr/encoder.int8.onnx"),
                decoder: p("MODELS::qwen3-asr/decoder.int8.onnx"),
                tokenizer: p("MODELS::qwen3-asr/tokenizer"),
            },
            tokens: String::new(), // Qwen3 loads tokens from the tokenizer dir
            ..Default::default()
        };
        self
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

/// ONNX-backed Stage1 recognizer (Silero VAD + streaming Zipformer + batch ASR via the single
/// [`OnnxRuntimeManager`]). Thread-safe: the ring is shared with the ingest thread; the
/// consume loop runs on the caller's thread.
pub struct OnnxStage1Recognizer {
    mgr: Arc<OnnxRuntimeManager>,
    /// Batch ASR as a trait object: local OnnxAsr (from `mgr`) or remote HttpAsr. Streaming/VAD
    /// stay in `mgr` (always local sherpa).
    batch_asr: Arc<dyn AsrProvider>,
    ring: Arc<Mutex<AudioRing>>,
    /// Merge-window gap (s) — see [`Stage1Config::merge_gap_s`].
    merge_gap_s: f64,
    active: Arc<AtomicBool>,
    /// The PCM store: segments' clips live here by id until their window settles.
    audio_store: Arc<AudioStore>,
}

impl OnnxStage1Recognizer {
    /// Build models from `cfg`, warm them, spawn the scout→ring ingest thread.
    pub fn new(cfg: Stage1Config) -> Result<Self> {
        // Batch ASR: Local → OnnxAsr lives in the mgr; Remote → HttpAsr (mgr skips .asr()).
        let mgr = match &cfg.asr_kind {
            ProviderKind::Local => Arc::new(
                OnnxRuntimeManager::builder()
                    .vad(cfg.vad.clone())
                    .asr(cfg.asr.clone())
                    .streaming_asr(cfg.streaming.clone())
                    .build()?,
            ),
            ProviderKind::Remote { .. } => Arc::new(
                OnnxRuntimeManager::builder()
                    .vad(cfg.vad.clone())
                    .streaming_asr(cfg.streaming.clone())
                    .build()?, // no local batch ASR — remote HttpAsr handles it
            ),
        };
        let batch_asr: Arc<dyn AsrProvider> = match &cfg.asr_kind {
            ProviderKind::Local => {
                Arc::clone(mgr.asr().expect("local asr just loaded")) as Arc<dyn AsrProvider>
            }
            ProviderKind::Remote { endpoint } => Arc::new(HttpAsr::new(endpoint.clone())),
        };
        mgr.warm();
        let ring = Arc::new(Mutex::new(AudioRing::new(cfg.ring_cap_samples)));
        spawn_ingest(Arc::clone(&ring), &cfg.scout_addr, Arc::clone(&cfg.active))?;
        Ok(Self {
            mgr,
            ring,
            merge_gap_s: cfg.merge_gap_s,
            active: cfg.active,
            audio_store: Arc::new(AudioStore::new(DEFAULT_CAP_SAMPLES)),
            batch_asr,
        })
    }

    /// Use an already-loaded [`OnnxRuntimeManager`] (e.g. shared with another stage); spawns the
    /// ingest thread against `cfg.scout_addr`.
    pub fn new_with_mgr(mgr: Arc<OnnxRuntimeManager>, cfg: Stage1Config) -> Result<Self> {
        let batch_asr: Arc<dyn AsrProvider> = match &cfg.asr_kind {
            ProviderKind::Local => {
                Arc::clone(mgr.asr().expect("local mgr must carry the batch ASR")) as Arc<dyn AsrProvider>
            }
            ProviderKind::Remote { endpoint } => Arc::new(HttpAsr::new(endpoint.clone())),
        };
        let ring = Arc::new(Mutex::new(AudioRing::new(cfg.ring_cap_samples)));
        spawn_ingest(Arc::clone(&ring), &cfg.scout_addr, Arc::clone(&cfg.active))?;
        Ok(Self {
            mgr,
            ring,
            merge_gap_s: cfg.merge_gap_s,
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
/// `active` controls whether it connects (see [`ScoutAudioSource::with_active`]).
fn spawn_ingest(
    ring: Arc<Mutex<AudioRing>>,
    scout_addr: &str,
    active: Arc<AtomicBool>,
) -> Result<()> {
    let src = ScoutAudioSource::with_active(scout_addr.to_string(), WINDOW, active);
    thread::Builder::new()
        .name("aura-stage1-ingest".into())
        .spawn(move || {
            src.stream(
                move |win| ring.lock().unwrap().push(win),
                Duration::from_secs(2),
            );
        })?;
    Ok(())
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

    /// VAD StartOfSpeech at wall-clock `at`: settle the open window FIRST when the gap since
    /// its last segment ≥ `merge_gap_s` (returns the settled spans, if any), then start a new
    /// in-progress segment — in the same window when the gap was short, else a fresh window.
    fn on_sos(&mut self, at: f64) -> (Option<SettledSpans>, WindowId, SegmentId) {
        let settled = self.settle_if_gap(at);
        let window_id = match &self.open {
            Some(w) => w.window_id, // short gap — same window continues
            None => {
                let id = self.next_win_id;
                self.next_win_id += 1;
                self.open = Some(OpenWindow { window_id: id, segments: Vec::new(), active: false });
                id
            }
        };
        let segment_id = self.next_seg_id;
        self.next_seg_id += 1;
        self.open.as_mut().expect("window just ensured").active = true;
        (settled, window_id, segment_id)
    }

    /// Settle the open window iff the gap from `sos_at` back to its last segment ≥ merge_gap.
    fn settle_if_gap(&mut self, sos_at: f64) -> Option<SettledSpans> {
        let gap = {
            let w = self.open.as_ref()?;
            let last = w.segments.last()?;
            sos_at - last.end_s
        };
        if gap >= self.merge_gap_s {
            self.take_open()
        } else {
            None
        }
    }

    /// Record a completed segment (already transcribed by the recognizer). Returns the Batch
    /// payload: the window id + ALL its segments so far — the payload IS the window, so
    /// Stage2 stays stateless (no separate left-boundary bookkeeping to desync).
    fn on_eos(&mut self, seg: VadSegment) -> (WindowId, Vec<VadSegment>) {
        let w = self.open.as_mut().expect("EOS without an open window");
        w.active = false;
        w.segments.push(seg);
        (w.window_id, w.segments.clone())
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
    /// TRAILING window finalizes. Suppressed while a segment is in progress.
    fn check_settle(&mut self, now: f64) -> Option<SettledSpans> {
        let w = self.open.as_ref()?;
        if w.active {
            return None;
        }
        let last = w.segments.last()?;
        if now - last.end_s >= self.merge_gap_s {
            self.take_open()
        } else {
            None
        }
    }

    fn take_open(&mut self) -> Option<SettledSpans> {
        self.open.take().map(|w| SettledSpans { window_id: w.window_id, segments: w.segments })
    }

    /// The ids the segment currently being spoken WILL get: the open window's id (or the next
    /// one when nothing is open) + the next segment id. Used to key live `Interim` partials —
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
}

impl ActiveSession {
    fn new(stream: StreamingSession) -> Self {
        Self {
            stream,
            frames_since_partial: 0,
            last_partial: String::new(),
            last_change: Instant::now(),
            fed: 0,
        }
    }
}

impl Stage1Recognizer for OnnxStage1Recognizer {
    // TODO: 该函数静默阻塞线程，使用睡眠轮询的方式；需要整改成异步非阻塞模式；
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

        loop {
            // Connection toggle: when the scout connection is paused, skip VAD/ASR (the ingest
            // thread also stops feeding the ring, so it drains to empty shortly).
            if !self.active.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            // Settle the trailing window: no follow-up segment came for merge_gap — it's done.
            // (Suppressed while speech is in progress inside the tracker.) The wait is
            // unavoidable — you must observe the gap to know it ended — but the per-segment
            // Batch results have been showing live text throughout, so it doesn't lag.
            let now_s = start.elapsed().as_secs_f64();
            if let Some(settled) = tracker.check_settle(now_s) {
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
                    if let Some(since) = ring_empty_since {
                        let has_partial =
                            sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
                        if since.elapsed() > Duration::from_secs(2) && has_partial {
                            debug!("ring empty > 2s during speech — feeding silence to force VAD EOS");
                            vec![0i16; WINDOW] // synthetic silence frame → VAD will fire EOS
                        } else {
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                    } else {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                }
            };
            frames_in += 1;
            if last_diag.elapsed() >= Duration::from_secs(3) {
                let rlen = self.ring.lock().unwrap().len();
                // NOTE: `is_speaking()` is useless with this retroactive VAD (only true for
                // the instant a segment pops) — the meaningful live signal is the partial.
                let has_partial = sess.as_ref().map(|a| !a.last_partial.is_empty()).unwrap_or(false);
                debug!(frames = frames_in, ring = rlen, has_partial, "stage1 diag");
                last_diag = Instant::now();
            }

            // (1) live streaming partial — the session is fed CONTINUOUSLY (D1 adaptation:
            //     this VAD's SOS is retroactive, so gating on speech-start is impossible);
            //     see [`ActiveSession`]. Throttle to ~0.5s, only on change. Keyed by the
            //     tracker's prospective ids (authoritative grouping comes with Batch).
            //     NOT a Stage2 input (D2).
            if let (Some(asr), Some(a)) = (sasr, sess.as_mut()) {
                a.stream.accept_waveform(sr as i32, &frame);
                a.fed += 1;
                a.frames_since_partial += 1;
                if a.frames_since_partial >= PARTIAL_EVERY_FRAMES {
                    let partial = asr.decode_and_result(&a.stream);
                    if !partial.is_empty() && partial != a.last_partial {
                        let (window_id, segment_id) = tracker.prospective();
                        on_event(Stage1Event::Interim {
                            window_id,
                            segment_id,
                            partial: partial.clone(),
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
            for ev in self.mgr.vad().unwrap().push_frame(&frame) {
                match ev.kind {
                    VadEventKind::StartOfSpeech => {
                        let now = start.elapsed().as_secs_f64();
                        let (settled, window_id, segment_id) = tracker.on_sos(now);
                        // A big gap settled the previous window FIRST — emit it before the
                        // new segment lands in the next window.
                        if let Some(s) = settled {
                            emit_window_edge(s, &self.audio_store, &*self.batch_asr, sr, on_event);
                            sess = sasr.map(|asr| ActiveSession::new(asr.create_session()));
                        }
                        cur_seg = segment_id;
                        let _ = window_id; // authoritative windowing comes from on_eos
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
                        // One batch pass over exactly this segment's PCM (edge-extended by
                        // the VAD). Err (remote network) and empty text both map to None.
                        let batch_text = self
                            .batch_asr
                            .recognize(&ev.pcm, sr)
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
                        let start_s = (end_s - ev.pcm.len() as f64 / sr as f64).max(0.0);
                        let seg = VadSegment {
                            id: cur_seg,
                            audio_id: self.audio_store.insert(ev.pcm),
                            start_s,
                            end_s,
                            streaming_text,
                            batch_text,
                        };
                        let (window_id, segments) = tracker.on_eos(seg);
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
                        on_event(Stage1Event::Batch { window_id, segments });
                    }
                }
            }
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
        let (settled, w1, s1) = t.on_sos(0.0);
        assert!(settled.is_none());
        let (w, segs) = t.on_eos(seg(s1, 0.0, 0.5));
        assert_eq!((w, segs.len()), (w1, 1));

        // gap 1.0−0.5 = 0.5 < 2.5 → same window, second segment.
        let (settled, w2, s2) = t.on_sos(1.0);
        assert!(settled.is_none(), "short gap must NOT settle");
        assert_eq!(w2, w1, "same window continues");
        let (w, segs) = t.on_eos(seg(s2, 1.0, 1.5));
        assert_eq!((w, segs.len()), (w1, 2), "both segments in one window");
    }

    #[test]
    fn big_gap_settles_previous_window_and_opens_new_one() {
        let mut t = WindowTracker::new(2.5);
        let (_, w1, s1) = t.on_sos(0.0);
        t.on_eos(seg(s1, 0.0, 0.5));
        // gap 5.0−0.5 = 4.5 ≥ 2.5 → settle w1, open w2.
        let (settled, w2, s2) = t.on_sos(5.0);
        let s = settled.expect("big gap settles the previous window");
        assert_eq!(s.window_id, w1);
        assert_eq!(s.segments.len(), 1);
        assert_ne!(w2, w1, "a fresh window opens");
        assert!(w2 > w1, "window ids are monotonic");
        let (w, segs) = t.on_eos(seg(s2, 5.0, 5.5));
        assert_eq!((w, segs.len()), (w2, 1));
    }

    #[test]
    fn settle_timeout_closes_trailing_window() {
        let mut t = WindowTracker::new(2.5);
        let (_, w1, s1) = t.on_sos(0.0);
        t.on_eos(seg(s1, 0.0, 0.5));
        assert!(t.check_settle(2.0).is_none(), "2.0 − 0.5 = 1.5 < 2.5, not yet");
        let s = t.check_settle(3.0).expect("3.0 − 0.5 = 2.5 ≥ merge_gap → settle");
        assert_eq!(s.window_id, w1);
        assert!(t.check_settle(10.0).is_none(), "nothing open anymore");
    }

    #[test]
    fn active_segment_suppresses_settle_timeout() {
        // Regression guard: a long following segment must not be mistaken for "no
        // continuation" and force-split the window mid-speech.
        let mut t = WindowTracker::new(2.5);
        let (_, _, s1) = t.on_sos(0.0);
        t.on_eos(seg(s1, 0.0, 0.5));
        let (_, _, _s2) = t.on_sos(1.0); // gap 0.5 < 2.5 → same window, speaking again
        assert!(t.check_settle(100.0).is_none(), "active segment ⇒ settle suppressed");
    }

    #[test]
    fn merge_gap_zero_makes_every_segment_its_own_window() {
        let mut t = WindowTracker::new(0.0);
        let (_, w1, s1) = t.on_sos(0.0);
        t.on_eos(seg(s1, 0.0, 0.5));
        // Any gap ≥ 0 settles: at the next SOS…
        let (settled, w2, _) = t.on_sos(0.6);
        assert_eq!(settled.expect("gap 0.1 ≥ 0 settles").window_id, w1);
        assert_ne!(w2, w1);
        // …and the settle timeout fires immediately after an EOS too.
        let (_, _, s3) = t.on_sos(10.0);
        t.on_eos(seg(s3, 10.0, 10.5));
        assert!(t.check_settle(10.5).is_some(), "now − end = 0 ≥ 0 → settle");
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
        let (_, w1, _) = t.on_sos(0.0);
        t.drop_active();
        // Window stays open (same id), nothing recorded; settle timeout has nothing to close.
        let (_, w2, _) = t.on_sos(1.0);
        assert_eq!(w1, w2);
        assert!(t.check_settle(100.0).is_none(), "no segments → nothing to settle");
    }
}
