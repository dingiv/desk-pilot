//! Stage1Executor — encapsulates the Stage1 "noodle": the audio ring + omni-scout ingest
//! thread + Silero VAD + per-segment streaming sessions + per-segment batch passes + the
//! window tracker. Owns ALL the loop state. It runs the consume loop internally and emits
//! [`Stage1Event`]s — it does NOT touch files or run Stage2 (that's the composer's job, in
//! `aura-core::Pipeline`).
//!
//! Boundary paradigm (docs/aura/vad-segment-model.md): the VAD gap (`min_silence`) closes a
//! [`VadSegment`] (its own streaming session per D1 + one batch pass, packed as a `Batch`
//! event); the merge window (`merge_gap`) closes a [`VadWindow`] (concatenated PCM re-run
//! through the batch model, packed as a `WindowEdge` event). PCM lives in the
//! [`AudioStore`] by id — events carry ids + texts only, plus the window's shared
//! `Arc<Vec<i16>>` assembled once at settle.
//!
//! ```ignore
//! let exec = OnnxStage1Executor::new(Stage1Config::new(scout_addr))?;
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
use tracing::debug;

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

/// Config for [`OnnxStage1Executor`] — paths + params for the VAD, batch ASR, and streaming ASR,
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

/// A Stage1 executor: audio in → [`Stage1Event`]s out. `run` blocks forever (drives the
/// ingest+consume loop) and invokes `on_event` for each interim partial / settled segment /
/// closed window.
pub trait Stage1Executor {
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) -> !;
}

/// ONNX-backed Stage1 executor (Silero VAD + streaming Zipformer + batch ASR via the single
/// [`OnnxRuntimeManager`]). Thread-safe: the ring is shared with the ingest thread; the
/// consume loop runs on the caller's thread.
pub struct OnnxStage1Executor {
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

impl OnnxStage1Executor {
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

    /// The PCM store this executor owns — clips are addressable by [`AudioId`] until their
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
// The executor owns the ASR side (sessions, batch passes, the AudioStore); this tracker owns
// ONLY the boundary math — which segment belongs to which window, and when a window closes.

/// The open window: its settled segments + whether a segment is in progress (SOS seen,
/// EOS pending). The in-progress segment's id/timing live executor-side ([`ActiveSession`]);
/// the tracker only needs "is one active" for settle suppression.
struct OpenWindow {
    window_id: WindowId,
    segments: Vec<VadSegment>,
    active: bool,
}

/// A window closed by a big gap or the settle-timeout — the executor turns this into a
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

    /// Record a completed segment (already transcribed by the executor). Returns the Batch
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
    let batch_text = batch_asr.recognize(&pcm, sr).ok().filter(|t| !t.trim().is_empty());
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

/// Executor-side mirror of the tracker's active segment: the per-segment streaming session
/// (D1 — created at SOS, finalized and dropped at EOS) + its partial-throttle state.
struct ActiveSession {
    segment_id: SegmentId,
    window_id: WindowId,
    start_s: f64,
    stream: StreamingSession,
    frames_since_partial: u32,
    last_partial: String,
}

impl Stage1Executor for OnnxStage1Executor {
    // TODO: 该函数静默阻塞线程，使用睡眠轮询的方式；需要整改成异步非阻塞模式；
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) -> ! {
        let sr = 16000u32;
        let start = Instant::now();
        let mut last_diag = Instant::now();
        let mut frames_in = 0u64;

        let sasr = self.mgr.streaming_asr();
        let mut sess: Option<ActiveSession> = None;
        let mut ring_empty_since: Option<Instant> = None;
        let mut tracker = WindowTracker::new(self.merge_gap_s);

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
                let speaking = self.mgr.vad().map(|v| v.is_speaking()).unwrap_or(false);
                debug!(frames = frames_in, ring = rlen, speaking, "stage1 diag");
                last_diag = Instant::now();
            }

            // (1) per-segment streaming partial (D1: the session exists only while a segment
            //     is in progress — SOS..EOS). Throttle to ~0.5s, only on change.
            //     `decode_and_result` drains ALL pending chunks (is_ready loop), so the
            //     hypothesis stays caught-up with real-time. NOT a Stage2 input (D2).
            if let (Some(asr), Some(a)) = (sasr, sess.as_mut()) {
                a.stream.accept_waveform(sr as i32, &frame);
                a.frames_since_partial += 1;
                if a.frames_since_partial >= PARTIAL_EVERY_FRAMES {
                    let partial = asr.decode_and_result(&a.stream);
                    if !partial.is_empty() && partial != a.last_partial {
                        on_event(Stage1Event::Interim {
                            window_id: a.window_id,
                            segment_id: a.segment_id,
                            partial: partial.clone(),
                            at_s: start.elapsed().as_secs_f64(),
                        });
                        a.last_partial = partial;
                    }
                    a.frames_since_partial = 0;
                }
            }

            // (2) VAD boundaries → WindowTracker → Batch (per segment) / WindowEdge (settle).
            for ev in self.mgr.vad().unwrap().push_frame(&frame) {
                match ev.kind {
                    VadEventKind::StartOfSpeech => {
                        let now = start.elapsed().as_secs_f64();
                        if sess.is_some() {
                            debug!("SOS while a segment is active — replacing the session");
                        }
                        let (settled, window_id, segment_id) = tracker.on_sos(now);
                        // A big gap settled the previous window FIRST — emit it before the
                        // new segment's partials start flowing.
                        if let Some(s) = settled {
                            emit_window_edge(s, &self.audio_store, &*self.batch_asr, sr, on_event);
                        }
                        // Fresh per-segment session (D1): create at SOS, finalize at EOS.
                        sess = sasr.map(|asr| ActiveSession {
                            segment_id,
                            window_id,
                            start_s: now,
                            stream: asr.create_session(),
                            frames_since_partial: 0,
                            last_partial: String::new(),
                        });
                    }
                    VadEventKind::EndOfSpeech => {
                        let end_s = start.elapsed().as_secs_f64();
                        // Finalize the segment's streaming session → its streaming_text.
                        let a = sess.take();
                        let streaming_text = match (sasr, a.as_ref()) {
                            (Some(asr), Some(a)) => asr.finalize_and_result(&a.stream),
                            _ => String::new(),
                        };
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
                        let seg = VadSegment {
                            id: a.as_ref().map(|a| a.segment_id).unwrap_or_default(),
                            audio_id: self.audio_store.insert(ev.pcm),
                            start_s: a.as_ref().map(|a| a.start_s).unwrap_or(end_s),
                            end_s,
                            streaming_text,
                            batch_text,
                        };
                        let (window_id, segments) = tracker.on_eos(seg);
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
