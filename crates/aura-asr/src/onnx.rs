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
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tracing::info;

/// Silero window = 512 samples = 32 ms @ 16 kHz (fixed by the model).
pub const WINDOW: usize = 512;
/// Audio sample rate the whole Stage1 pipeline runs at.
pub const SAMPLE_RATE: u32 = 16000;
/// Recall window for segment edge-extension: must span the longest segment (max_speech 28 s)
/// plus the trailing-silence observation (~1 s) plus the margin — 60 s covers all of it.
const RECALL_S: usize = 60;

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
    asr: Option<Arc<OnnxAsr>>,
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

    /// Access the (batch) ASR, if configured. Behind an `Arc` so the executor can hand it
    /// out as `Arc<dyn AsrProvider>` for the local/remote swap.
    pub fn asr(&self) -> Option<&Arc<OnnxAsr>> {
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
        let asr = self.asr.map(OnnxAsr::new).transpose()?.map(Arc::new);
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
    /// ★Segment edge-extension (seconds) at VAD boundaries, default 0.3 (0 = off).
    /// Silero cuts the soft onset (samples BEFORE its probability first crosses `threshold`)
    /// and the fading coda (samples AFTER it drops below) off every segment — the batch ASR
    /// hears the detection window, not the real speech, so a sentence can lose its first or
    /// last character. The extension re-pads both edges from the executor's recall buffer
    /// (see [`OnnxVad`]): head up to `edge_margin_s` before the onset, tail up to
    /// `edge_margin_s` after the end — the fade-out has already streamed through by the time
    /// EOS fires (`min_silence` of observation), so both recoveries are free. This is the
    /// fix for "missing first/last word" on merged utterances.
    pub edge_margin_s: f32,
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
            edge_margin_s: 0.3,
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
    /// Sliding recall of every sample fed to the VAD (last `RECALL_S` seconds). At EOS the true
    /// speech extends past Silero's threshold-crossing boundaries (soft onsets and fading codas
    /// live BELOW `threshold` and get cut from the segment) — the edge extension pulls those
    /// samples back out of recall, so the batch ASR hears the real speech, not the detection
    /// window. This is what fixes "first/last character dropped" at merge boundaries.
    recall: VecDeque<i16>,
    /// Total samples fed to the VAD so far — aligns `SpeechSegment::start` (absolute sample
    /// offset in the fed stream) with `recall`'s window.
    total_fed: u64,
    /// Absolute end sample of the last emitted segment (INCLUDING its tail extension) — the
    /// next segment's head extension stops here so adjacent segments never overlap (no
    /// duplicated audio in the merger's concatenated PCM).
    prev_seg_end: Option<i64>,
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
            sample_rate: SAMPLE_RATE as i32,
            ..Default::default()
        };
        let det = VoiceActivityDetector::create(&mc, cfg.buffer_seconds)
            .ok_or_else(|| anyhow::anyhow!("sherpa-onnx VoiceActivityDetector::create failed"))?;
        Ok(OnnxVad {
            inner: Mutex::new(Vad {
                det,
                speaking: false,
                recall: VecDeque::with_capacity(RECALL_S * SAMPLE_RATE as usize),
                total_fed: 0,
                prev_seg_end: None,
            }),
            cfg,
        })
    }

    /// Feed exactly `window_size` i16 samples. Returns any SOS/EOS events — the EOS segment's
    /// PCM is edge-extended (see [`VadConfig::edge_margin_s`]) with the real audio around
    /// Silero's threshold-crossing boundaries, pulled from this VAD's recall buffer.
    pub fn push_frame(&self, frame: &[i16]) -> Vec<VadEvent> {
        assert_eq!(frame.len(), self.cfg.window_size as usize, "OnnxVad expects window_size frames");
        let samples: Vec<f32> = frame.iter().map(|&s| s as f32 / 32768.0).collect();
        let mut inner = self.inner.lock().unwrap();
        inner.total_fed += frame.len() as u64;
        inner.recall.extend(frame.iter().copied());
        while inner.recall.len() > RECALL_S * SAMPLE_RATE as usize {
            inner.recall.pop_front();
        }
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
                // Edge extension: the segment covers only [onset, fade-start] (Silero's
                // threshold crossings); the real speech extends past both. Recall holds every
                // fed sample, so both edges are recoverable — head before the onset, tail after
                // the segment end (EOS fired `min_silence` AFTER the fade, so it's in recall).
                let ext = segment_extension(
                    seg.start() as i64,
                    pcm.len() as i64,
                    inner.total_fed,
                    inner.recall.len(),
                    self.cfg.edge_margin_s,
                    inner.prev_seg_end,
                );
                let mut extended =
                    Vec::with_capacity(ext.head_len() + pcm.len() + ext.tail_len());
                let recall_lo = inner.total_fed as i64 - inner.recall.len() as i64;
                if ext.head_len() > 0 {
                    copy_recall(
                        &mut extended,
                        inner.recall.as_slices(),
                        (ext.head_lo - recall_lo) as usize,
                        (ext.head_hi - recall_lo) as usize,
                    );
                }
                extended.extend_from_slice(&pcm);
                if ext.tail_len() > 0 {
                    copy_recall(
                        &mut extended,
                        inner.recall.as_slices(),
                        (ext.tail_lo - recall_lo) as usize,
                        (ext.tail_hi - recall_lo) as usize,
                    );
                }
                // Track the EXTENDED end — the next segment's head extension must not reach
                // back into this segment's tail (they would overlap in the merged PCM).
                inner.prev_seg_end = Some(seg.start() as i64 + extended.len() as i64);
                events.push(VadEvent { kind: VadEventKind::EndOfSpeech, pcm: extended });
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

// ── Segment edge-extension (pure helpers) ──────────────────────────────────

/// Extension window for a VAD segment spanning absolute samples `[start, start+len)`: the
/// samples Silero's threshold cut off — soft onset before the first above-threshold frame,
/// fading coda after the last one. `[head_lo, head_hi)` is prepended, `[tail_lo, tail_hi)`
/// appended. All indices are absolute (sample position in the fed audio stream).
struct SegmentExtent {
    head_lo: i64,
    head_hi: i64,
    tail_lo: i64,
    tail_hi: i64,
}

impl SegmentExtent {
    fn head_len(&self) -> usize {
        (self.head_hi - self.head_lo).max(0) as usize
    }
    fn tail_len(&self) -> usize {
        (self.tail_hi - self.tail_lo).max(0) as usize
    }
}

/// Compute the edge-extension window — see [`VadConfig::edge_margin_s`]. `total_fed` /
/// `recall_len` bound the recall buffer's reach (samples before `total_fed - recall_len` are
/// gone); `prev_seg_end` (the previous segment's EXTENDED end) stops the head extension from
/// reaching back into it — at a merge boundary the previous tail is already in the
/// accumulator, and duplicating it would make the batch ASR repeat text.
fn segment_extension(
    start: i64,
    len: i64,
    total_fed: u64,
    recall_len: usize,
    margin_s: f32,
    prev_seg_end: Option<i64>,
) -> SegmentExtent {
    let margin = (margin_s * SAMPLE_RATE as f32) as i64;
    let total = total_fed as i64;
    let recall_lo = total - recall_len as i64;
    let end = start + len;
    SegmentExtent {
        head_lo: (start - margin).max(recall_lo).max(prev_seg_end.unwrap_or(i64::MIN)),
        head_hi: start.min(total),
        tail_lo: end.min(total),
        tail_hi: (end + margin).min(total),
    }
}

/// Copy samples `[s, e)` of the recall buffer (absolute indices) into `out`, given the two
/// `VecDeque::as_slices` halves.
fn copy_recall(out: &mut Vec<i16>, (a, b): (&[i16], &[i16]), s: usize, e: usize) {
    let n = e - s;
    if s + n <= a.len() {
        out.extend_from_slice(&a[s..s + n]);
    } else if s < a.len() {
        out.extend_from_slice(&a[s..]);
        out.extend_from_slice(&b[..n - (a.len() - s)]);
    } else {
        out.extend_from_slice(&b[s - a.len()..s - a.len() + n]);
    }
}

#[cfg(test)]
mod vad_edge_tests {
    use super::*;

    #[test]
    fn recovers_onset_and_coda() {
        // margin 0.3 s = 4800 samples @ 16 kHz. Segment [10000, 18000); recall covers it all.
        let e = segment_extension(10000, 8000, 30000, 30000, 0.3, None);
        assert_eq!(e.head_lo, 5200, "head reaches back one margin");
        assert_eq!(e.head_hi, 10000);
        assert_eq!(e.head_len(), 4800);
        assert_eq!(e.tail_lo, 18000);
        assert_eq!(e.tail_hi, 22800, "tail reaches forward one margin");
        assert_eq!(e.tail_len(), 4800);
    }

    #[test]
    fn zero_margin_is_noop() {
        let e = segment_extension(10000, 8000, 30000, 30000, 0.0, None);
        assert_eq!(e.head_len(), 0);
        assert_eq!(e.tail_len(), 0);
    }

    #[test]
    fn head_stops_at_previous_segment() {
        // Previous segment's extended end = 19000; this one starts at 21000. A full head margin
        // (→16200) would duplicate the gap audio in the merger — clamp to 19000.
        let e = segment_extension(21000, 8000, 40000, 40000, 0.3, Some(19000));
        assert_eq!(e.head_lo, 19000);
        assert_eq!(e.head_len(), 2000);
    }

    #[test]
    fn clamps_to_recall_coverage() {
        // recall covers [20000, 30000). start 21000 is inside → head gets only the covered
        // 1000 samples (the rest of the margin is gone); tail extends up to the fed total.
        let e = segment_extension(21000, 8000, 30000, 10000, 0.3, None);
        assert_eq!(e.head_lo, 20000);
        assert_eq!(e.head_len(), 1000);
        assert_eq!(e.tail_lo, 29000);
        assert_eq!(e.tail_hi, 30000);
        assert_eq!(e.tail_len(), 1000);

        // segment entirely before recall coverage (long segment, start already evicted) → no
        // head extension at all; tail (recent, always covered) still extends to the fed total.
        let e = segment_extension(19000, 8000, 30000, 10000, 0.3, None);
        assert_eq!(e.head_len(), 0);
        assert_eq!(e.tail_lo, 27000);
        assert_eq!(e.tail_hi, 30000);
        assert_eq!(e.tail_len(), 3000);
    }

    #[test]
    fn copy_spans_both_recall_slices() {
        let mut out = Vec::new();
        copy_recall(&mut out, (&[1, 2, 3][..], &[4, 5, 6][..]), 2, 5);
        assert_eq!(out, vec![3, 4, 5], "crosses the slice boundary");
        let mut out = Vec::new();
        copy_recall(&mut out, (&[1, 2, 3][..], &[4, 5, 6][..]), 0, 6);
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6]);
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
    /// ONNX Runtime execution provider for the BATCH ASR only: `"cpu"` (default) | `"cuda"`.
    /// Empty/`"cpu"` ⇒ sherpa's default (CPU). Anything else is passed through as the provider
    /// name. Requires the CUDA-enabled sherpa shared lib (see `.cargo/config.toml` lib symlinks);
    /// with the CPU-only lib, a non-cpu value will fail at `OfflineRecognizer::create`.
    pub provider: String,
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
            num_threads: 8, // sweet spot on 8C/16T (Zen5); 2 wastes cores, 16 (SMT) contends on mem bw
            provider: "cpu".into(),
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
        //
        // `provider`: batch ASR only — VAD/streaming stay CPU. Empty/`"cpu"` ⇒ sherpa default.
        let provider = if cfg.provider.trim().is_empty() || cfg.provider == "cpu" {
            None
        } else {
            Some(cfg.provider.clone())
        };
        let mut mc = sherpa_onnx::OfflineModelConfig {
            tokens: if cfg.tokens.is_empty() { None } else { Some(cfg.tokens.clone()) },
            num_threads: cfg.num_threads,
            provider,
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
        let text = stream.get_result().map(|r| r.text).unwrap_or_default();
        Ok(strip_qwen3_markers(&text))
    }
}

/// Qwen3-ASR occasionally leaks internal markers into its output
/// (`language Chinese<asr_text>文本</asr_text>`). Only strips when `<asr_text>` is present —
/// SenseVoice/Whisper never emit this tag, so it's a safe no-op for them.
fn strip_qwen3_markers(text: &str) -> String {
    let t = text.trim();
    if let Some(pos) = t.find("<asr_text>") {
        let after = &t[pos + "<asr_text>".len()..];
        return after.replace("</asr_text>", "").trim().to_string();
    }
    t.to_string()
}
