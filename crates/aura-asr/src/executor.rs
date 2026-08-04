//! Stage1Executor — encapsulates the Stage1 "noodle": the audio ring + omni-scout ingest
//! thread + Silero VAD + two-pass ASR (streaming Zipformer partials + batch SenseVoice final).
//! Owns ALL the loop state that used to live in `stage12_live.rs`'s `main()`. It runs the
//! consume loop internally and emits [`Stage1Event`]s — it does NOT touch files or run Stage2
//! (that's the composer's job, in `aura-core::Pipeline`).
//!
//! ```ignore
//! let exec = OnnxStage1Executor::new(Stage1Config { scout_addr, vad, asr, streaming, ring_cap_samples })?;
//! exec.run(&mut |ev| match ev {
//!     Stage1Event::Interim { partial, .. } => println!("…{partial}"),
//!     Stage1Event::Action(Stage1Action::MergeBatch(u)) => stage2.calibrate(&u),
//!     Stage1Event::Action(Stage1Action::Batch(u)) => stage2.calibrate_provisional(&u),
//! });
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::debug;

use crate::buffer::AudioRing;
use crate::onnx::{
    AsrBackend, AsrConfig, OnlineAsr, OnnxRuntimeManager, StreamingAsrConfig, StreamingSession,
    VadConfig, WINDOW,
};
use crate::scout::ScoutAudioSource;
use crate::{Stage1Action, Stage1Event, Utterance, VadEventKind};
use dp_models::{http::HttpAsr, AsrProvider, ProviderKind};

/// Default ring capacity: 10 min @ 16 kHz mono (~19 MB).
const DEFAULT_RING_CAP: usize = 16_000 * 600;
/// Streaming-partial decode cadence: every N windows (~0.5s @ 32ms Silero windows).
const PARTIAL_EVERY_FRAMES: u32 = 15;
/// Diligent Stage2: calibrate the streaming partial this often during active speech. The partial
/// is already text (no batch ASR needed) — just a Stage2 LLM call — so this is cheap and keeps
/// the live candidate calibrated-fresh instead of idling between rare VAD fragments.
const STREAM_CALIBRATE_INTERVAL: Duration = Duration::from_millis(1000);

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
    /// ★Segment-merge gap (seconds) — the UPPER bound of the medium-interval multi-sentence
    /// merge window. VAD fires EOS on every pause ≥ `min_silence` (kept low, ~1.0s, so batch
    /// recognition kicks in fast); the [`SegmentMerger`] then absorbs a following segment into
    /// the pending utterance when the inter-speech silence < this. The lower bound is implicit:
    /// `min_silence` is what splits segments in the first place, so the effective window is
    /// (min_silence, merge_gap) ≈ 1–5s — fragments forcibly split by a medium pause stitch back
    /// into one paragraph, and batch ASR re-runs on the merged PCM to UPDATE the sentence. Only
    /// a gap ≥ this (or no new speech for this long) settles → Final. Decouples "VAD sensitivity"
    /// from "what's one utterance" — VAD stays reactive, merging repairs the fragmentation. 0 → no merging.
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
/// ingest+consume loop) and invokes `on_event` for each interim partial / finalized utterance.
pub trait Stage1Executor {
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) -> !;
}

/// ONNX-backed Stage1 executor (Silero VAD + streaming Zipformer + batch SenseVoice via the
/// single [`OnnxRuntimeManager`]). Thread-safe: the ring is shared with the ingest thread; the
/// consume loop runs on the caller's thread.
pub struct OnnxStage1Executor {
    mgr: Arc<OnnxRuntimeManager>,
    /// Batch ASR as a trait object: local OnnxAsr (from `mgr`) or remote HttpAsr. Streaming/VAD
    /// stay in `mgr` (always local sherpa).
    batch_asr: Arc<dyn AsrProvider>,
    ring: Arc<Mutex<AudioRing>>,
    /// Segment-merge gap (s) — see [`Stage1Config::merge_gap_s`].
    merge_gap_s: f64,
    active: Arc<AtomicBool>,
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
        Ok(Self { mgr, ring, merge_gap_s: cfg.merge_gap_s, active: cfg.active, batch_asr })
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
        Ok(Self { mgr, ring, merge_gap_s: cfg.merge_gap_s, active: cfg.active, batch_asr })
    }

    /// Access the underlying ONNX model manager (e.g. for diagnostics / direct ASR calls).
    pub fn manager(&self) -> &Arc<OnnxRuntimeManager> {
        &self.mgr
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

// ── Segment merging: stitch VAD fragments split by short pauses into one utterance ───────
// R2 from docs/aura/real-world-speech-design.md §1 (停顿碎片化). VAD fires EOS on every pause
// ≥ min_silence; the merger absorbs a following segment into the pending utterance when the
// inter-speech silence is < `merge_gap_s`. Each absorbed fragment re-runs batch ASR on the
// accumulated PCM and emits a `Batch` action (provisional, same seq — Stage2 recalibrates
// incrementally, the UI updates in place); the utterance only settles (a gap ≥ merge_gap_s, or
// no new speech for merge_gap_s) into a `MergeBatch` action (authoritative). So the UI sees one
// growing utterance instead of N fragments — "fragment anxiety" (R3) solved for free — and
// Stage2's calibrated text appears live (per fragment), not delayed until settle.

/// Accumulated audio + timing for an utterance that may still absorb more segments.
struct MergeAccum {
    pcm: Vec<i16>,
    /// Wall-clock (s since executor start) of the FIRST segment's SOS — the utterance's `at_s`.
    start_at: f64,
    /// Wall-clock of the LAST absorbed segment's speech end (its SOS + duration). The gap to the
    /// next segment's SOS is measured from here — the TRUE inter-speech silence, independent of
    /// `min_silence_duration` (EOS fires min_silence into the silence; SOS fires at onset).
    last_seg_end_at: f64,
}

/// A reference to a (possibly merged) audio buffer + its utterance start — what the caller
/// transcribes (batch ASR) to produce a Batch or MergeBatch action.
struct MergeRef {
    pcm: Vec<i16>,
    start_at: f64,
}

/// What [`SegmentMerger::on_eos`] produced: optionally a *settled* previous utterance (emit
/// Final), plus always the *current* in-progress utterance's accumulated audio (emit Batch).
struct EosOutcome {
    /// The previous utterance settled (gap ≥ merge_gap_s) — transcribe + emit Final. `None` when
    /// this segment was absorbed into the current utterance (or it's the first ever segment).
    settled: Option<MergeRef>,
    /// The current utterance's accumulated audio so far — transcribe + emit a `Batch` action
    /// (provisional).
    provisional: MergeRef,
}

/// Turns a VAD SOS/EOS stream into provisional Batch actions + settled MergeBatch actions,
/// merging fragments
/// whose inter-speech silence is shorter than `merge_gap_s`. Pure + unit-testable (no I/O). The
/// executor drives it: `on_sos`/`on_eos` per VAD event, `check_settle` every tick.
struct SegmentMerger {
    merge_gap_s: f64,
    sample_rate: f32,
    accum: Option<MergeAccum>,
    pending_sos: Option<f64>,
    /// A segment is in progress (SOS seen, EOS pending). The settle timeout is suppressed while
    /// true, so a long following segment isn't mistaken for "no continuation" and force-split.
    speaking: bool,
}

impl SegmentMerger {
    fn new(merge_gap_s: f64, sample_rate: u32) -> Self {
        Self {
            merge_gap_s,
            sample_rate: sample_rate as f32,
            accum: None,
            pending_sos: None,
            speaking: false,
        }
    }

    /// VAD StartOfSpeech at wall-clock `at` (s).
    fn on_sos(&mut self, at: f64) {
        self.pending_sos = Some(at);
        self.speaking = true;
    }

    /// VAD EndOfSpeech with the segment's PCM. Returns the settled previous utterance (if the gap
    /// ≥ merge_gap_s) plus the current utterance's accumulated audio (always — every EOS starts
    /// or extends a pending utterance, so there's always a provisional to emit). `eos_at` is the
    /// wall-clock when EOS fired.
    fn on_eos(&mut self, pcm: Vec<i16>, eos_at: f64) -> EosOutcome {
        self.speaking = false;
        let seg_dur = (pcm.len() as f32 / self.sample_rate) as f64;
        let sos = self.pending_sos.take().unwrap_or_else(|| (eos_at - seg_dur).max(0.0));
        let seg_end = sos + seg_dur;

        let absorb = self
            .accum
            .as_ref()
            .map(|acc| sos - acc.last_seg_end_at < self.merge_gap_s)
            .unwrap_or(false);

        let settled = if absorb {
            // same utterance: grow the accumulator. Nothing settles.
            let acc = self.accum.as_mut().unwrap();
            acc.pcm.extend(&pcm);
            acc.last_seg_end_at = seg_end;
            None
        } else {
            // gap too big (or first ever segment): settle the previous, start a new accumulator
            // from this segment.
            let prev = self.accum.take().map(|p| MergeRef { pcm: p.pcm, start_at: p.start_at });
            self.accum = Some(MergeAccum { pcm, start_at: sos, last_seg_end_at: seg_end });
            prev
        };

        let provisional = {
            let acc = self.accum.as_ref().expect("accum just set");
            MergeRef { pcm: acc.pcm.clone(), start_at: acc.start_at }
        };
        EosOutcome { settled, provisional }
    }

    /// Settle-timeout probe (call every loop tick with the current wall-clock). If the pending
    /// utterance has been silent (no active speech) for ≥ merge_gap_s, finalize it. Suppressed
    /// while a segment is in progress (`speaking`).
    fn check_settle(&mut self, now: f64) -> Option<MergeRef> {
        if self.speaking {
            return None;
        }
        let acc = self.accum.as_ref()?;
        if now - acc.last_seg_end_at >= self.merge_gap_s {
            let acc = self.accum.take().unwrap();
            Some(MergeRef { pcm: acc.pcm, start_at: acc.start_at })
        } else {
            None
        }
    }
}

/// Provisional transcript of an in-progress (merging) utterance: batch-recognize the accumulated
/// PCM only (the streaming session is still live — do NOT finalize it). Returns `None` when the
/// batch result is empty (silence/noise). `seq` is left 0 for the caller (`idx + 1`).
fn transcribe_provisional(
    pcm: &[i16],
    start_at: f64,
    batch_asr: &dyn AsrProvider,
    sr: u32,
) -> Option<Utterance> {
    let raw_text = batch_asr.recognize(pcm, sr).unwrap_or_default();
    if raw_text.trim().is_empty() {
        return None;
    }
    let duration_ms = (pcm.len() as f32 / sr as f32) * 1000.0;
    Some(Utterance {
        seq: 0,
        raw_text,
        streaming_text: String::new(),
        duration_ms,
        at_s: start_at,
        pcm: pcm.to_vec(),
    })
}

/// Final transcript of a settled utterance: finalize the streaming session (which spans the whole
/// merged utterance) → hotword-biased `streaming_text`, then batch-recognize the merged PCM →
/// authoritative `raw_text`, and start a fresh streaming session for the next utterance (a reset
/// would bleed encoder context across boundaries). Returns `None` when both transcripts are empty.
/// `seq` is left 0 for the caller (`idx`).
fn transcribe_final(
    pcm: Vec<i16>,
    start_at: f64,
    batch_asr: &dyn AsrProvider,
    sasr: Option<&OnlineAsr>,
    stream_sess: &mut Option<StreamingSession>,
    sr: u32,
) -> Option<Utterance> {
    let streaming_text = match (sasr, stream_sess.as_ref()) {
        (Some(s), Some(sess)) => s.finalize_and_result(sess),
        _ => String::new(),
    };
    *stream_sess = sasr.map(|s| s.create_session());
    let raw_text = batch_asr.recognize(&pcm, sr).unwrap_or_default();
    if raw_text.trim().is_empty() && streaming_text.trim().is_empty() {
        return None;
    }
    let duration_ms = (pcm.len() as f32 / sr as f32) * 1000.0;
    Some(Utterance { seq: 0, raw_text, streaming_text, duration_ms, at_s: start_at, pcm })
}

impl Stage1Executor for OnnxStage1Executor {
    // TODO: 该函数静默阻塞线程，使用睡眠轮询的方式；需要整改成异步非阻塞模式；
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) -> ! {
        let sr = 16000u32;
        let start = Instant::now();
        let mut last_diag = Instant::now();
        let mut frames_in = 0u64;
        let mut idx = 0u64;

        // Streaming session for the two-pass live path. Spans a whole MERGED utterance (it is NOT
        // reset on each VAD EOS anymore — that would chop partials at every pause); it is replaced
        // with a fresh session only when the utterance SETTLES (inside `transcribe_merged`). `reset`
        // would leave encoder context that bleeds across boundaries; a fresh session starts clean.
        // Decoding is `is_ready`-gated inside `decode_and_result`, so a fresh session is safe to
        // poll immediately (no warmup dance needed).
        let sasr = self.mgr.streaming_asr();
        let mut stream_sess = sasr.map(|s| s.create_session());
        let mut last_partial = String::new();
        let mut last_stream_calibrate: Option<Instant> = None;
        let mut frames_since_partial = 0u32;
        let mut ring_empty_since: Option<Instant> = None;
        // Segment merger — absorbs VAD fragments split by short pauses (< merge_gap_s) into one
        // utterance; a Final is emitted only when the utterance settles. See [`SegmentMerger`].
        let mut merger = SegmentMerger::new(self.merge_gap_s, sr);

        loop {
            // Connection toggle: when the scout connection is paused, skip VAD/ASR (the ingest
            // thread also stops feeding the ring, so it drains to empty shortly).
            if !self.active.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            // Settle any pending utterance whose trailing silence has exceeded the merge gap — no
            // follow-up segment came, so the utterance is done. (Suppressed while speech is in
            // progress inside the merger.) Runs every active tick; this is how the *trailing*
            // utterance finalizes (every other utterance settles when the next segment's gap ≥
            // merge_gap). The wait is unavoidable — you must observe the gap to know it ended —
            // but the streaming partial + provisional Batch results have been showing live text
            // throughout, so it doesn't lag.
            let now_s = start.elapsed().as_secs_f64();
            if let Some(settled) = merger.check_settle(now_s) {
                if let Some(mut u) = transcribe_final(
                    settled.pcm,
                    settled.start_at,
                    &*self.batch_asr,
                    sasr,
                    &mut stream_sess,
                    sr,
                ) {
                    idx += 1;
                    u.seq = idx;
                    on_event(Stage1Event::Action(Stage1Action::MergeBatch(u)));
                    last_partial.clear();
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
                    // Ring empty: if > 2s AND we have streaming partials (were speaking),
                    // feed silence to VAD so it fires EOS naturally — prevents the scenario
                    // where audio source drops mid-utterance and VAD never evaluates silence.
                    ring_empty_since.get_or_insert_with(Instant::now);
                    if let Some(since) = ring_empty_since {
                        if since.elapsed() > Duration::from_secs(2) && !last_partial.is_empty() {
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

            // (1) streaming partial (two-pass: live path) — throttle to ~0.5s, only on change.
            // `decode_and_result` drains ALL pending chunks (is_ready loop), so the hypothesis
            // stays caught-up with real-time instead of falling further behind on every poll.
            if let (Some(s), Some(sess)) = (sasr, stream_sess.as_ref()) {
                sess.accept_waveform(sr as i32, &frame);
                frames_since_partial += 1;
                if frames_since_partial >= PARTIAL_EVERY_FRAMES {
                    let partial = s.decode_and_result(sess);
                    if !partial.is_empty() && partial != last_partial {
                        on_event(Stage1Event::Interim {
                            seq: idx + 1, // the in-progress utterance's prospective seq
                            partial: partial.clone(),
                            at_s: start.elapsed().as_secs_f64(),
                        });
                        // Diligent Stage2: periodically calibrate the streaming partial text
                        // (the partial is already text → no batch ASR, just Stage2). Keeps the
                        // live candidate calibrated-fresh during continuous speech, instead of
                        // waiting for a rare VAD fragment. seq = idx+1 (same as Interim/Final).
                        if last_stream_calibrate
                            .map_or(true, |t| t.elapsed() >= STREAM_CALIBRATE_INTERVAL)
                        {
                            last_stream_calibrate = Some(Instant::now());
                            on_event(Stage1Event::Action(Stage1Action::Batch(Utterance {
                                seq: idx + 1,
                                raw_text: partial.clone(),
                                streaming_text: String::new(),
                                duration_ms: 0.0,
                                at_s: start.elapsed().as_secs_f64(),
                                pcm: Vec::new(),
                            })));
                        }
                        last_partial = partial;
                    }
                    frames_since_partial = 0;
                }
            }

            // (2) VAD segment boundaries → SegmentMerger → Batch action (provisional) + settle →
            //     MergeBatch action (authoritative). Each absorbed fragment re-runs batch ASR on
            //     the accumulated PCM → `Stage1Action::Batch` (provisional, same seq, Stage2
            //     recalibrates incrementally). When the gap ≥ merge_gap the previous utterance
            //     settles → `Stage1Action::MergeBatch` (authoritative). Streaming partials + Batch
            //     keep the same seq across the whole merge, so the UI sees one growing utterance,
            //     not N fragments. The streaming session spans the whole merged utterance (NOT
            //     reset per EOS); it's finalized only at settle (inside `transcribe_final`).
            for ev in self.mgr.vad().unwrap().push_frame(&frame) {
                match ev.kind {
                    VadEventKind::StartOfSpeech => {
                        merger.on_sos(start.elapsed().as_secs_f64());
                    }
                    VadEventKind::EndOfSpeech => {
                        let eos_at = start.elapsed().as_secs_f64();
                        let outcome = merger.on_eos(ev.pcm.clone(), eos_at);
                        // (a) previous utterance settled (gap ≥ merge_gap) → authoritative Final.
                        if let Some(prev) = outcome.settled {
                            if let Some(mut u) = transcribe_final(
                                prev.pcm,
                                prev.start_at,
                                &*self.batch_asr,
                                sasr,
                                &mut stream_sess,
                                sr,
                            ) {
                                idx += 1;
                                u.seq = idx;
                                on_event(Stage1Event::Action(Stage1Action::MergeBatch(u)));
                                last_partial.clear();
                            }
                        }
                        // (b) current utterance's accumulated audio → provisional Batch action (seq = idx+1,
                        //     the in-progress utterance's prospective seq, matching the Interim partials).
                        if let Some(mut u) = transcribe_provisional(
                            &outcome.provisional.pcm,
                            outcome.provisional.start_at,
                            &*self.batch_asr,
                            sr,
                        ) {
                            u.seq = idx + 1;
                            on_event(Stage1Event::Action(Stage1Action::Batch(u)));
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

    /// `dur_s` seconds of zero PCM at 16 kHz (content is irrelevant to the merger — only the
    /// sample COUNT drives segment duration / gap math).
    fn pcm(dur_s: f64) -> Vec<i16> {
        vec![0i16; (16000.0 * dur_s) as usize]
    }

    #[test]
    fn absorbs_short_gap_settles_on_long_gap_and_timeout() {
        let mut m = SegmentMerger::new(1.5, 16000);
        // seg1: sos 0.0, 0.5s speech → provisional = seg1, nothing settled.
        m.on_sos(0.0);
        let o = m.on_eos(pcm(0.5), 0.5);
        assert!(o.settled.is_none(), "first segment settles nothing");
        assert_eq!(o.provisional.pcm.len(), 8000);

        // seg2: sos 1.0 → gap 0.5 < 1.5 → absorb. Provisional = merged seg1+seg2.
        m.on_sos(1.0);
        let o = m.on_eos(pcm(0.5), 1.5);
        assert!(o.settled.is_none(), "short gap absorbs, nothing settles");
        assert_eq!(o.provisional.pcm.len(), 16000, "provisional grew to seg1+seg2");

        // seg3: sos 4.0 → gap 4.0−1.5 = 2.5 ≥ 1.5 → settle merged seg1+seg2, start seg3.
        m.on_sos(4.0);
        let o = m.on_eos(pcm(0.5), 4.5);
        let settled = o.settled.expect("long gap settles the previous utterance");
        assert_eq!(settled.start_at, 0.0, "settled utterance keeps the FIRST segment's start");
        assert_eq!(settled.pcm.len(), 16000, "settled = seg1+seg2 merged");
        assert_eq!(o.provisional.pcm.len(), 8000, "provisional is the new seg3");
        assert_eq!(o.provisional.start_at, 4.0);

        // seg3 pending (start 4.0, end 4.5); settle-timeout finalizes it.
        assert!(m.check_settle(5.0).is_none(), "5.0 − 4.5 = 0.5 < 1.5, not yet");
        let settled = m.check_settle(6.0).expect("6.0 − 4.5 = 1.5 ≥ merge_gap → settle");
        assert_eq!(settled.pcm.len(), 8000);
        assert_eq!(settled.start_at, 4.0);
    }

    #[test]
    fn speaking_suppresses_settle_during_long_following_segment() {
        // Regression guard: without the `speaking` flag, a long following segment would trip the
        // settle timeout mid-segment and wrongly split one utterance in two.
        let mut m = SegmentMerger::new(1.5, 16000);
        m.on_sos(0.0);
        m.on_eos(pcm(0.5), 0.5);
        m.on_sos(1.0); // gap will be 0.5 < 1.5 → absorb; but EOS hasn't come yet (speaking)
        assert!(m.check_settle(100.0).is_none(), "speaking ⇒ settle suppressed even at t=100");
        let o = m.on_eos(pcm(0.5), 1.5);
        assert!(o.settled.is_none(), "absorbed, not split");
    }

    #[test]
    fn long_gap_splits_into_two_utterances() {
        let mut m = SegmentMerger::new(1.5, 16000);
        m.on_sos(0.0);
        m.on_eos(pcm(0.5), 0.5);
        m.on_sos(3.0); // gap 3.0 − 0.5 = 2.5 ≥ 1.5 → settle seg1, start seg2
        let o = m.on_eos(pcm(0.5), 3.5);
        let settled = o.settled.expect("gap ≥ merge_gap must settle");
        assert_eq!(settled.pcm.len(), 8000);
        assert_eq!(settled.start_at, 0.0);
    }

    #[test]
    fn merge_gap_zero_disables_merging() {
        // 0 ⇒ the gap is never < 0, so nothing absorbs — every segment settles at the next EOS.
        let mut m = SegmentMerger::new(0.0, 16000);
        m.on_sos(0.0);
        let o = m.on_eos(pcm(0.5), 0.5);
        assert!(o.settled.is_none(), "first segment settles nothing");
        m.on_sos(0.6);
        let o = m.on_eos(pcm(0.5), 1.1);
        assert!(o.settled.is_some(), "merge_gap=0 ⇒ every gap settles the previous");
    }
}
