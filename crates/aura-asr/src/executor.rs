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
//!     Stage1Event::Final(u) => stage2.calibrate(&u),
//! });
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::debug;

use crate::buffer::AudioRing;
use crate::onnx::{AsrBackend, AsrConfig, OnnxRuntimeManager, StreamingAsrConfig, VadConfig, WINDOW};
use crate::scout::ScoutAudioSource;
use crate::{Asr, Stage1Event, Utterance, VadEventKind};

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
    /// Shared connection toggle (see [`ScoutAudioSource::with_active`]). Flip to false to stop
    /// ingesting from scout (does NOT kill scout). Defaults to true.
    pub active: Arc<AtomicBool>,
}

impl Stage1Config {
    /// Sensible defaults — model paths resolved via `shared` namespace `MODELS` (declared in
    /// this crate's `Cargo.toml` `[package.metadata.shared]`). Dev: `<workspace>/native/models/`;
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
    ring: Arc<Mutex<AudioRing>>,
    active: Arc<AtomicBool>,
}

impl OnnxStage1Executor {
    /// Build models from `cfg`, warm them, spawn the scout→ring ingest thread.
    pub fn new(cfg: Stage1Config) -> Result<Self> {
        let mgr = Arc::new(
            OnnxRuntimeManager::builder()
                .vad(cfg.vad)
                .asr(cfg.asr)
                .streaming_asr(cfg.streaming)
                .build()?,
        );
        mgr.warm();
        let ring = Arc::new(Mutex::new(AudioRing::new(cfg.ring_cap_samples)));
        spawn_ingest(Arc::clone(&ring), &cfg.scout_addr, Arc::clone(&cfg.active))?;
        Ok(Self { mgr, ring, active: cfg.active })
    }

    /// Use an already-loaded [`OnnxRuntimeManager`] (e.g. shared with another stage); spawns the
    /// ingest thread against `cfg.scout_addr`.
    pub fn new_with_mgr(mgr: Arc<OnnxRuntimeManager>, cfg: Stage1Config) -> Result<Self> {
        let ring = Arc::new(Mutex::new(AudioRing::new(cfg.ring_cap_samples)));
        spawn_ingest(Arc::clone(&ring), &cfg.scout_addr, Arc::clone(&cfg.active))?;
        Ok(Self { mgr, ring, active: cfg.active })
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

impl Stage1Executor for OnnxStage1Executor {
    // TODO: 该函数静默阻塞线程，使用睡眠轮询的方式；需要整改成异步非阻塞模式；
    fn run(&self, on_event: &mut dyn FnMut(Stage1Event)) -> ! {
        let sr = 16000u32;
        let start = Instant::now();
        let mut last_diag = Instant::now();
        let mut frames_in = 0u64;
        let mut idx = 0u64;

        // Streaming session for the two-pass live path. Replaced (not reset) at each VAD EOS —
        // `reset` leaves encoder context that bleeds across segment boundaries; a fresh session
        // starts with zero context. Decoding is `is_ready`-gated inside `decode_and_result`, so a
        // fresh session is safe to poll immediately (no warmup dance needed).
        let sasr = self.mgr.streaming_asr();
        let mut stream_sess = sasr.map(|s| s.create_session());
        let mut last_partial = String::new();
        let mut frames_since_partial = 0u32;

        loop {
            // Connection toggle: when the scout connection is paused, skip VAD/ASR (the ingest
            // thread also stops feeding the ring, so it drains to empty shortly).
            if !self.active.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            // drain one Silero window (512 samples = 32ms) when available
            let frame = {
                let mut g = self.ring.lock().unwrap();
                if !g.has_frame(WINDOW) {
                    drop(g);
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                g.drain(WINDOW)
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
                        last_partial = partial;
                    }
                    frames_since_partial = 0;
                }
            }

            // (2) VAD segment boundary → batch final → emit Final(Utterance)
            for ev in self.mgr.vad().unwrap().push_frame(&frame) {
                if !matches!(ev.kind, VadEventKind::EndOfSpeech) {
                    continue;
                }
                let at_s = start.elapsed().as_secs_f64();
                let duration_ms = (ev.pcm.len() as f32 / sr as f32) * 1000.0;

                // capture streaming final (hotword-biased) for comparison — `finalize_and_result`
                // flushes end-of-input + drains every pending chunk, so the tail is complete —
                // then replace the session with a FRESH one (reset leaves encoder context that
                // bleeds across segment boundaries — a new session starts clean).
                let streaming_text = if let (Some(s), Some(sess)) = (sasr, stream_sess.as_ref()) {
                    s.finalize_and_result(sess)
                } else {
                    String::new()
                };
                stream_sess = sasr.map(|s| s.create_session());

                // batch final (authoritative) — what Stage2 routes on
                let raw_text = self
                    .mgr
                    .asr()
                    .expect("Stage1 executor requires a batch ASR")
                    .recognize(&ev.pcm, sr)
                    .unwrap_or_default();

                if raw_text.trim().is_empty() && streaming_text.trim().is_empty() {
                    continue;
                }
                idx += 1;
                on_event(Stage1Event::Final(Utterance {
                    seq: idx,
                    raw_text,
                    streaming_text,
                    duration_ms,
                    at_s,
                    pcm: ev.pcm.clone(),
                }));
                last_partial.clear();
            }
        }
    }
}
