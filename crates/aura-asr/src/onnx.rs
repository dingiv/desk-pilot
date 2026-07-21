//! onnx — the single ONNX-runtime owner for Stage1 (ONNX ecosystem side of the dual-runtime
//! architecture, see docs/aura/runtime-selection.md). All ONNX models — VAD (Silero), ASR (SenseVoice),
//! and future streaming ASR / TTS — are loaded, warmed, and owned by [`OnnxRuntimeManager`],
//! which holds them through the OFFICIAL `sherpa-onnx` crate (one onnxruntime instance for all).
//!
//! Usage:
//! ```ignore
//! let mgr = OnnxRuntimeManager::builder()
//!     .vad(VadConfig { model: ".../silero_vad.onnx".into(), ..Default::default() })
//!     .asr(AsrConfig { model: ".../model.int8.onnx".into(), tokens: ".../tokens.txt".into(), ..Default::default() })
//!     .build()?;
//! mgr.warm();
//!
//! // then:
//! mgr.vad().unwrap().push_frame(&frame);
//! mgr.asr().unwrap().recognize(&pcm, 16000)?;
//! ```

use crate::{Asr, VadEvent, VadEventKind};

use anyhow::Result;
use sherpa_onnx::{
    OfflineQwen3ASRModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineSenseVoiceModelConfig,
    OnlineModelConfig, OnlineRecognizer, OnlineRecognizerConfig, OnlineTransducerModelConfig,
    SileroVadModelConfig, VadModelConfig, VoiceActivityDetector,
};
use std::sync::Mutex;
use tracing::info;

/// Silero window = 512 samples = 32 ms @ 16 kHz (fixed by the model).
pub const WINDOW: usize = 512;

// ── Streaming ASR: Zipformer transducer (real-time, partial results with correction) ──

/// Config for the streaming Zipformer transducer model (3 files: encoder/decoder/joiner + tokens).
#[derive(Debug, Clone)]
pub struct StreamingAsrConfig {
    pub encoder: String,
    pub decoder: String,
    pub joiner: String,
    pub tokens: String,
    pub num_threads: i32,
    /// Path to `bpe.vocab` (text vocab exported from `bpe.model` via sentencepiece, format
    /// `piece score` per line). sherpa uses it to tokenize RAW-TEXT hotwords itself when
    /// `modeling_unit = cjkchar+bpe`. Generate it with: see docs/aura/stage2-optimization.md §2.1.
    pub bpe_vocab: String,
    /// Hotword phrases (RAW TEXT — sherpa tokenizes them via bpe_vocab + modeling_unit).
    pub hotwords: Vec<String>,
    /// Score boost for hotword paths in beam search (typical 1.0-2.0).
    pub hotwords_score: f32,
}

impl Default for StreamingAsrConfig {
    fn default() -> Self {
        StreamingAsrConfig {
            encoder: String::new(),
            decoder: String::new(),
            joiner: String::new(),
            tokens: String::new(),
            num_threads: 2,
            bpe_vocab: String::new(),
            hotwords: Vec::new(),
            hotwords_score: 2.0,
        }
    }
}

/// A single streaming recognition session. Feed audio with `accept_waveform`, poll partial
/// results with `result`, and call `input_finished` at the end of an utterance. Each session
/// is independent (one per VAD segment).
pub struct StreamingSession {
    stream: sherpa_onnx::OnlineStream,
}

impl StreamingSession {
    /// Feed i16 PCM samples (any length; the engine buffers internally).
    pub fn accept_waveform(&self, sample_rate: i32, pcm: &[i16]) {
        let samples: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
        self.stream.accept_waveform(sample_rate, &samples);
    }

    /// Signal end of utterance — flushes the internal decoder state.
    pub fn input_finished(&self) {
        self.stream.input_finished();
    }
}

/// Streaming ASR via Zipformer transducer. Creates independent sessions per utterance;
/// each session produces partial text that **updates (corrects) as more audio arrives** — the
/// "phone input method" effect. Thread-safe: one recognizer, multiple sessions.
pub struct OnlineAsr {
    rec: Mutex<OnlineRecognizer>,
}

impl OnlineAsr {
    pub fn new(cfg: StreamingAsrConfig) -> Result<Self> {
        // Hotwords: RAW TEXT, one per line (sherpa tokenizes them itself). ASCII is uppercased to
        // match this bilingual model's uppercase English vocab (it emits ROS/READY, never ros/ready).
        // sherpa needs `modeling_unit=cjkchar+bpe` + `bpe_vocab=<bpe.vocab>` to run its BPE encoder
        // over these — see https://k2-fsa.github.io/sherpa/onnx/hotwords/index.html (cjkchar+bpe).
        // bpe.vocab is exported from bpe.model via sentencepiece (format `piece score` per line).
        let hotwords_str: String = cfg
            .hotwords
            .iter()
            .map(|p| p.trim())
            .filter(|t| !t.is_empty())
            .map(|t| {
                t.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { c })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let rc = OnlineRecognizerConfig {
            model_config: OnlineModelConfig {
                transducer: OnlineTransducerModelConfig {
                    encoder: Some(cfg.encoder),
                    decoder: Some(cfg.decoder),
                    joiner: Some(cfg.joiner),
                },
                tokens: Some(cfg.tokens),
                // Required for sherpa to tokenize raw-text hotwords on this bilingual model.
                modeling_unit: Some("cjkchar+bpe".into()),
                bpe_vocab: if cfg.bpe_vocab.is_empty() { None } else { Some(cfg.bpe_vocab) },
                num_threads: cfg.num_threads,
                ..Default::default()
            },
            enable_endpoint: true,
            rule1_min_trailing_silence: 2.4,
            // Contextual biasing REQUIRES modified_beam_search (greedy_search has no beam to bias).
            decoding_method: Some("modified_beam_search".into()),
            max_active_paths: 4,
            // Config-level hotwords (matches the official cjkchar+bpe streaming example, which is
            // proven to bias: LIBR→礼拜二, 平凡→频繁). Raw text, tokenized by sherpa via bpe_vocab.
            hotwords_buf: if hotwords_str.is_empty() { None } else { Some(hotwords_str.into_bytes()) },
            hotwords_score: cfg.hotwords_score,
            ..Default::default()
        };
        info!(
            hotwords = %if cfg.hotwords.is_empty() { "none".to_string() } else { cfg.hotwords.join(", ") },
            score = cfg.hotwords_score,
            "streaming-asr hotwords (modeling-unit=cjkchar+bpe)"
        );
        let rec = OnlineRecognizer::create(&rc)
            .ok_or_else(|| anyhow::anyhow!("OnlineRecognizer::create failed"))?;
        Ok(OnlineAsr { rec: Mutex::new(rec) })
    }

    /// Start a new recognition session (one per utterance/VAD segment). Contextual biasing (if
    /// configured) is baked into the recognizer and applies to every stream automatically.
    pub fn create_session(&self) -> StreamingSession {
        let rec = self.rec.lock().unwrap();
        StreamingSession { stream: rec.create_stream() }
    }

    /// Decode ALL pending audio and return the current best hypothesis (partial text). Call this
    /// after each `accept_waveform` to get the latest partial — it may differ from the previous
    /// one (earlier text gets corrected as more context arrives).
    ///
    /// sherpa's `decode()` decodes **one chunk-step** (~320ms); the official pattern is to drain
    /// with `while is_ready { decode }`. A single `decode` per call made the hypothesis fall
    /// ~160ms further behind real-time on every poll — after 20s of continuous speech the partial
    /// lagged seconds, and the backlog was silently discarded when the session was replaced at
    /// EOS (the "hidden audio loss" between the two passes). The `is_ready` gate also makes this
    /// safe on a fresh session (never decodes before a full chunk is buffered — the bare `decode`
    /// used to trip sherpa's C++ `GetFrames` assertion).
    pub fn decode_and_result(&self, session: &StreamingSession) -> String {
        let rec = self.rec.lock().unwrap();
        while rec.is_ready(&session.stream) {
            rec.decode(&session.stream);
        }
        rec.get_result(&session.stream)
            .map(|r| r.text)
            .unwrap_or_default()
    }

    /// Finalize an utterance: signal end-of-input (flushes the encoder's tail chunk — without it
    /// the last sub-chunk of audio is never decoded), drain every pending step, and return the
    /// final text. The session is spent afterwards — create a fresh one for the next utterance.
    pub fn finalize_and_result(&self, session: &StreamingSession) -> String {
        session.input_finished();
        self.decode_and_result(session)
    }

    /// Check if the engine's internal endpointing detected end-of-utterance.
    pub fn is_endpoint(&self, session: &StreamingSession) -> bool {
        let rec = self.rec.lock().unwrap();
        rec.is_endpoint(&session.stream)
    }

    /// Whether the stream has more frames queued to decode. Call decode_and_result in a loop
    /// until this returns false.
    pub fn is_ready(&self, session: &StreamingSession) -> bool {
        let rec = self.rec.lock().unwrap();
        rec.is_ready(&session.stream)
    }

    /// Reset the session's state (start fresh within the same session, e.g. after endpoint).
    pub fn reset(&self, session: &StreamingSession) {
        let rec = self.rec.lock().unwrap();
        rec.reset(&session.stream);
    }
}

// ── OnnxRuntimeManager ─────────────────────────────────────────────────────

/// The single owner of all ONNX-side models (the ONNX half of the dual-runtime architecture).
/// Built via [`OnnxRuntimeManager::builder()`]; all configured models load upfront at `build()`.
/// Thread-safe — share via `Arc<OnnxRuntimeManager>`. Each inner model has its own Mutex.
pub struct OnnxRuntimeManager {
    vad: Option<OnnxVad>,
    asr: Option<OnnxAsr>,
    streaming_asr: Option<OnlineAsr>,
    // future: tts — add field here when it lands
}

impl OnnxRuntimeManager {
    pub fn builder() -> OnnxRuntimeManagerBuilder {
        OnnxRuntimeManagerBuilder { vad: None, asr: None, streaming_asr: None }
    }

    /// Access the VAD, if configured. Returns `None` if `.vad(cfg)` was not called on the builder.
    pub fn vad(&self) -> Option<&OnnxVad> {
        self.vad.as_ref()
    }

    /// Access the (batch) ASR, if configured.
    pub fn asr(&self) -> Option<&OnnxAsr> {
        self.asr.as_ref()
    }

    /// Access the streaming ASR (Zipformer transducer), if configured.
    pub fn streaming_asr(&self) -> Option<&OnlineAsr> {
        self.streaming_asr.as_ref()
    }

    /// Run a trivial inference through every loaded model — triggers any lazy GPU/cuDNN
    /// initialisation (JIT compile) so the first real inference isn't slow.
    pub fn warm(&self) {
        if let Some(vad) = &self.vad {
            let silence = vec![0i16; WINDOW];
            let _ = vad.push_frame(&silence);
        }
        if let Some(asr) = &self.asr {
            let silence = vec![0i16; 1600]; // 0.1s of silence
            let _ = asr.recognize(&silence, 16000);
        }
        if let Some(sasr) = &self.streaming_asr {
            let session = sasr.create_session();
            // Streaming Zipformer needs >= decoder_chunk_size frames (≈160ms) before a decode can
            // run — a single 100ms chunk trips `GetFrames` (too few frames). Feed several chunks so
            // the warmup decode (and its lazy GPU/JIT init) succeeds.
            for _ in 0..4 {
                session.accept_waveform(16000, &[0i16; 1600]);
            }
            let _ = sasr.decode_and_result(&session);
        }
    }
}

/// Builder for [`OnnxRuntimeManager`]. Chain `.vad()` / `.asr()` to configure which models to load,
/// then `.build()` to load them all.
pub struct OnnxRuntimeManagerBuilder {
    vad: Option<VadConfig>,
    asr: Option<AsrConfig>,
    streaming_asr: Option<StreamingAsrConfig>,
}

impl OnnxRuntimeManagerBuilder {
    pub fn vad(mut self, cfg: VadConfig) -> Self {
        self.vad = Some(cfg);
        self
    }
    pub fn asr(mut self, cfg: AsrConfig) -> Self {
        self.asr = Some(cfg);
        self
    }
    pub fn streaming_asr(mut self, cfg: StreamingAsrConfig) -> Self {
        self.streaming_asr = Some(cfg);
        self
    }

    /// Load all configured models. Errors propagate (e.g. missing model file → build fails fast).
    pub fn build(self) -> Result<OnnxRuntimeManager> {
        let vad = self.vad.map(OnnxVad::new).transpose()?;
        let asr = self.asr.map(OnnxAsr::new).transpose()?;
        let streaming_asr = self.streaming_asr.map(OnlineAsr::new).transpose()?;
        Ok(OnnxRuntimeManager { vad, asr, streaming_asr })
    }
}

// ── VAD: Silero via the official sherpa-onnx crate (no stall, unlike archived sherpa-rs) ──

/// Tunable VAD params. Defaults mirror Silero v5 + a hangover-friendly endpointing.
#[derive(Debug, Clone)]
pub struct VadConfig {
    pub model: String,
    pub threshold: f32,
    pub min_silence_duration: f32, // seconds
    pub min_speech_duration: f32,  // seconds
    pub max_speech_duration: f32,  // seconds (force-split very long utterances)
    pub window_size: i32,          // samples (512 = 32ms @ 16kHz, fixed by Silero)
    pub buffer_seconds: f32,       // internal segment accumulator
}

impl Default for VadConfig {
    fn default() -> Self {
        VadConfig {
            model: String::new(),
            // Silero's stock threshold. 0.6 (the old value) crossed too late on soft onsets and
            // clipped the first syllable — the segment start only looks back ~64ms past the
            // min-speech probation window, so a late trigger = a cut head.
            threshold: 0.5,
            // Sentence pauses in lecture-style speech run 0.5–1.2s. At the old 1.5s they NEVER
            // ended a segment, so continuous speech always hit the max_speech force-split —
            // which sherpa performs in an eager mode (threshold 0.90 / min_silence 0.1s) that
            // cuts MID-WORD, producing severed fragments on both sides of the cut.
            min_silence_duration: 1.0,
            // Utterances shorter than this are discarded entirely by sherpa's state machine.
            // 0.5s swallowed short commands ("好", "停"); 0.3s keeps them.
            min_speech_duration: 0.3,
            // Force-split backstop only (natural pauses should split first, see min_silence).
            // SenseVoice is comfortable up to ~30s per batch.
            max_speech_duration: 28.0,
            window_size: 512,
            buffer_seconds: 60.0,
        }
    }
}

/// Neural Silero VAD. `push_frame` feeds exactly `window_size` i16 samples; returns SOS/EOS events
/// (EOS carries the full utterance PCM). Thread-safe (Mutex).
pub struct OnnxVad {
    inner: Mutex<Vad>,
    cfg: VadConfig,
}

struct Vad {
    det: VoiceActivityDetector,
    /// accumulated utterance (i16) for the current segment — sherpa returns f32 segments, we
    /// convert + keep so the consumer gets i16 like the rest of the pipeline.
    speaking: bool,
}

impl OnnxVad {
    pub fn new(cfg: VadConfig) -> Result<Self> {
        let mc = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(cfg.model.clone()),
                threshold: cfg.threshold,
                min_silence_duration: cfg.min_silence_duration,
                min_speech_duration: cfg.min_speech_duration,
                max_speech_duration: cfg.max_speech_duration,
                window_size: cfg.window_size,
            },
            sample_rate: 16000,
            ..Default::default()
        };
        let det = VoiceActivityDetector::create(&mc, cfg.buffer_seconds)
            .ok_or_else(|| anyhow::anyhow!("sherpa-onnx VoiceActivityDetector::create failed"))?;
        Ok(OnnxVad { inner: Mutex::new(Vad { det, speaking: false }), cfg })
    }

    /// Feed exactly `window_size` i16 samples. Returns any SOS/EOS events.
    pub fn push_frame(&self, frame: &[i16]) -> Vec<VadEvent> {
        assert_eq!(frame.len(), self.cfg.window_size as usize, "OnnxVad expects window_size frames");
        let samples: Vec<f32> = frame.iter().map(|&s| s as f32 / 32768.0).collect();
        let mut inner = self.inner.lock().unwrap();
        inner.det.accept_waveform(&samples);

        let mut events = Vec::new();
        while !inner.det.is_empty() {
            if let Some(seg) = inner.det.front() {
                if !inner.speaking {
                    inner.speaking = true;
                    events.push(VadEvent { kind: VadEventKind::StartOfSpeech, pcm: Vec::new() });
                }
                let pcm: Vec<i16> =
                    seg.samples().iter().map(|&f| (f * 32768.0).clamp(-32768.0, 32767.0) as i16).collect();
                events.push(VadEvent { kind: VadEventKind::EndOfSpeech, pcm });
                inner.speaking = false;
            }
            inner.det.pop();
        }
        events
    }

    pub fn flush(&self) -> Vec<VadEvent> {
        // sherpa's VAD has no explicit flush API. If the internal detector is stuck (speaking=true
        // but no segment emitted for a long time), the only way to clear it is to recreate it.
        // This flush is a no-op for now — the executor's consume loop handles stuck states via
        // the connection toggle (active=false clears everything on reconnect).
        Vec::new()
    }

    /// Whether the VAD currently thinks someone is speaking (for diagnostics).
    pub fn is_speaking(&self) -> bool {
        self.inner.lock().unwrap().speaking
    }
}

// ── ASR: offline recognizers (SenseVoice / Whisper / Paraformer / Qwen3-ASR) ─

/// Which batch ASR backend to use. All four are sherpa-onnx `OfflineRecognizer` —
/// same `recognize()` path, different model configs.
#[derive(Debug, Clone)]
pub enum AsrBackend {
    /// FunAudioLLM SenseVoice — fast, multi-language, emotion/event detection.
    SenseVoice { model: String, language: String },
    /// OpenAI Whisper (e.g. large-v3-turbo) — 99 languages, slower.
    Whisper { encoder: String, decoder: String, language: String },
    /// Alibaba Paraformer — strongest Chinese CER, fast.
    Paraformer { model: String },
    /// Alibaba Qwen3-Audio ASR — encoder-decoder LLM-style, strong multilingual.
    /// Autoregressive decode ⇒ slow on CPU (sherpa-onnx CPU-only here; fast once a CUDA
    /// build lands). `tokenizer` is a HuggingFace tokenizer DIRECTORY (vocab.json +
    /// merges.txt + tokenizer_config.json), NOT a single tokens file — so the shared
    /// `AsrConfig.tokens` is left empty for this backend.
    Qwen3Asr { conv_frontend: String, encoder: String, decoder: String, tokenizer: String },
}

#[derive(Debug, Clone)]
pub struct AsrConfig {
    pub backend: AsrBackend,
    pub tokens: String,
    pub use_itn: bool,
    pub num_threads: i32,
}

impl Default for AsrConfig {
    fn default() -> Self {
        AsrConfig {
            backend: AsrBackend::SenseVoice {
                model: String::new(),
                language: "auto".into(),
            },
            tokens: String::new(),
            use_itn: true,
            num_threads: 2,
        }
    }
}

/// Offline recognizer wrapping any sherpa-onnx backend (SenseVoice / Whisper / Paraformer).
/// `recognize()` runs one utterance through the model. Thread-safe.
pub struct OnnxAsr {
    rec: Mutex<OfflineRecognizer>,
}

impl OnnxAsr {
    pub fn new(cfg: AsrConfig) -> Result<Self> {
        // `tokens` is shared across backends EXCEPT Qwen3-ASR, which loads its vocab from the
        // HF tokenizer DIRECTORY (`backend.tokenizer`). Pass `None` when empty so sherpa doesn't
        // try to open a "" path.
        let mut mc = sherpa_onnx::OfflineModelConfig {
            tokens: if cfg.tokens.is_empty() { None } else { Some(cfg.tokens.clone()) },
            num_threads: cfg.num_threads,
            ..Default::default()
        };
        match &cfg.backend {
            AsrBackend::SenseVoice { model, language } => {
                mc.sense_voice = OfflineSenseVoiceModelConfig {
                    model: Some(model.clone()),
                    language: Some(language.clone()),
                    use_itn: cfg.use_itn,
                };
            }
            AsrBackend::Whisper { encoder, decoder, language } => {
                mc.whisper = sherpa_onnx::OfflineWhisperModelConfig {
                    encoder: Some(encoder.clone()),
                    decoder: Some(decoder.clone()),
                    language: Some(language.clone()),
                    task: Some("transcribe".into()),
                    tail_paddings: -1,
                    ..Default::default()
                };
            }
            AsrBackend::Paraformer { model } => {
                mc.paraformer = sherpa_onnx::OfflineParaformerModelConfig {
                    model: Some(model.clone()),
                };
            }
            AsrBackend::Qwen3Asr { conv_frontend, encoder, decoder, tokenizer } => {
                mc.qwen3_asr = OfflineQwen3ASRModelConfig {
                    conv_frontend: Some(conv_frontend.clone()),
                    encoder: Some(encoder.clone()),
                    decoder: Some(decoder.clone()),
                    tokenizer: Some(tokenizer.clone()),
                    ..Default::default()
                };
            }
        }
        let rc = OfflineRecognizerConfig {
            model_config: mc,
            ..Default::default()
        };
        let rec = OfflineRecognizer::create(&rc)
            .ok_or_else(|| anyhow::anyhow!("sherpa-onnx OfflineRecognizer::create failed"))?;
        Ok(OnnxAsr { rec: Mutex::new(rec) })
    }
}

impl Asr for OnnxAsr {
    fn recognize(&self, pcm: &[i16], sample_rate: u32) -> Result<String> {
        let rec = self.rec.lock().unwrap();
        let stream = rec.create_stream();
        let samples: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
        stream.accept_waveform(sample_rate as i32, &samples);
        rec.decode(&stream);
        Ok(stream.get_result().map(|r| r.text).unwrap_or_default())
    }
}
