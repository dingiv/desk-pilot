//! audio-aura-asr — Stage1 pure logic (no I/O): audio frame type, an energy VAD with a hysteresis
//! state machine (ported from livekit-agents Silero params), a VAD-gated segmenter (the
//! `StreamAdapter` pattern: accumulate frames between speech start/end, then batch-recognize), a
//! streaming/batch ASR trait, and the endpointing config. The daemon feeds 20ms frames in and gets
//! `SpeechEvent`s out; the real ASR (sherpa-onnx) plugs into the `Asr` trait.
//!
//! Design mirror: livekit `vad.py` (VADEvent SOS/EOS carrying accumulated frames),
//! `stt/stream_adapter.py` (VAD-gated batch→streaming), `voice/endpointing.py` (min/max delay).
//! See audio-aura/docs/livekit-port-notes.md.

use serde::Serialize;

pub mod buffer;
pub mod scout;
pub mod wav;
pub mod source;

/// The ONNX-runtime side of the dual-runtime architecture (see docs/runtime-selection.md):
/// VAD (Silero) + ASR (SenseVoice) via the OFFICIAL `sherpa-onnx` crate, which owns the single
/// onnxruntime instance. Gated behind `onnx` so the pure-DSP core builds without it.
#[cfg(feature = "onnx")]
pub mod onnx;

/// Stage1 stage-boundary abstractions: `Stage1Executor` (capture+VAD+two-pass ASR → events) +
/// the `Utterance`/`Stage1Event` data contract. The data types are always compiled (so the
/// Stage2 crate can reference `Utterance` without enabling the heavy `onnx` feature); the
/// executor impl + its config are `onnx`-gated.
#[cfg(feature = "onnx")]
pub mod executor;

// ── Stage1 → Stage2 data contract (NOT onnx-gated; plain data) ─────────────────
/// One finalized utterance from Stage1 (the batch final is authoritative; the streaming final
/// is the hotword-biased hypothesis for comparison). Stage2 calibrates `raw_text` (falling back
/// to `streaming_text` when the batch pass is empty).
#[derive(Debug, Clone)]
pub struct Utterance {
    /// Monotonic sequence number within the run.
    pub seq: u64,
    /// Batch SenseVoice final — the authoritative transcript Stage2 routes on.
    pub raw_text: String,
    /// Streaming Zipformer final (hotword-biased) — diagnostic / fallback when batch is empty.
    pub streaming_text: String,
    /// Utterance duration in milliseconds.
    pub duration_ms: f32,
    /// Wall-clock seconds since the executor started.
    pub at_s: f64,
    /// The segment's raw PCM (16 kHz mono S16LE) — for audio playback. Empty if not captured.
    pub pcm: Vec<i16>,
}

impl Utterance {
    /// The text Stage2 should calibrate on: batch final if non-empty, else streaming final.
    pub fn route_text(&self) -> &str {
        if self.raw_text.trim().is_empty() {
            &self.streaming_text
        } else {
            &self.raw_text
        }
    }
}

/// Events emitted by [`executor::Stage1Executor`]. Defined here (ungated) so downstream crates
/// can match on them without the `onnx` feature.
#[derive(Debug, Clone)]
pub enum Stage1Event {
    /// A live streaming partial (the "phone input method" evolving text).
    Interim { partial: String, at_s: f64 },
    /// A finalized utterance ready for Stage2 calibration.
    Final(Utterance),
}

/// One audio buffer: 16 kHz mono S16LE by default (matches omni-scout `/audio`).
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm: Vec<i16>,
}

impl AudioChunk {
    pub fn duration_ms(&self) -> f32 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        (self.pcm.len() as f32 / self.channels.max(1) as f32) / self.sample_rate as f32 * 1000.0
    }
}

/// Root-mean-square energy of a frame (proxy for loudness; the energy-VAD gate).
pub fn rms(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = frame.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / frame.len() as f64).sqrt() as f32
}

// ── speech events (port of livekit SpeechEventType) ─────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SpeechEventKind {
    StartOfSpeech,
    Interim,
    Final,
    EndOfSpeech,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeechEvent {
    pub kind: SpeechEventKind,
    /// Set on Interim/Final (the recognized text).
    pub text: Option<String>,
    /// Utterance duration in ms (set on Final).
    pub duration_ms: f32,
}

// ── VAD (port of livekit VADEvent + Silero state machine, energy-based) ─────────
#[derive(Debug, Clone, Copy)]
pub enum VadEventKind {
    StartOfSpeech,
    EndOfSpeech,
}

pub struct VadEvent {
    pub kind: VadEventKind,
    /// The accumulated utterance PCM (only on EndOfSpeech; empty on StartOfSpeech).
    pub pcm: Vec<i16>,
}

/// Config mirrors livekit Silero defaults (activation/min-durations), adapted to energy gating.
#[derive(Debug, Clone)]
pub struct VadConfig {
    pub sample_rate: u32,
    pub frame_ms: u32,        // 20ms frames
    pub rms_threshold: f32,   // energy gate (i16 scale); silence ~<200, speech ~>500
    pub min_speech_ms: u32,   // 50ms (Silero min_speech_duration)
    pub min_silence_ms: u32,  // 550ms (Silero min_silence_duration)
    pub prefix_pad_ms: u32,   // 300ms leading context kept before onset
}

impl Default for VadConfig {
    fn default() -> Self {
        VadConfig {
            sample_rate: 16000,
            frame_ms: 20,
            rms_threshold: 500.0,
            min_speech_ms: 50,
            min_silence_ms: 550,
            prefix_pad_ms: 300,
        }
    }
}

/// Energy VAD with hysteresis: enter speech after `min_speech_ms` of above-threshold frames, exit
/// after `min_silence_ms` of below-threshold frames. Accumulates the utterance (+ prefix pad) and
/// hands it back on EndOfSpeech (the frames a batch recognizer needs).
pub struct EnergyVad {
    cfg: VadConfig,
    speaking: bool,
    speech_ms: u32,
    silence_ms: u32,
    buffer: Vec<i16>,       // accumulated speech samples
    prefix: Vec<i16>,       // ring of recent pre-speech samples
    prefix_cap: usize,
}

impl EnergyVad {
    pub fn new(cfg: VadConfig) -> Self {
        let per_frame = (cfg.sample_rate as u64 * cfg.frame_ms as u64 / 1000) as usize;
        let prefix_frames = cfg.prefix_pad_ms / cfg.frame_ms.max(1);
        let prefix_cap = per_frame * prefix_frames as usize;
        EnergyVad {
            cfg,
            speaking: false,
            speech_ms: 0,
            silence_ms: 0,
            buffer: Vec::new(),
            prefix: Vec::with_capacity(prefix_cap + per_frame),
            prefix_cap,
        }
    }

    /// Feed one frame (expected `frame_ms` of mono S16LE). Returns a VadEvent on state transition.
    pub fn push_frame(&mut self, frame: &[i16]) -> Option<VadEvent> {
        let loud = rms(frame) >= self.cfg.rms_threshold;
        let fm = self.cfg.frame_ms;

        if self.speaking {
            self.buffer.extend_from_slice(frame);
        } else {
            // keep a bounded prefix of pre-speech audio so we don't clip the onset
            self.prefix.extend_from_slice(frame);
            if self.prefix.len() > self.prefix_cap {
                let drop = self.prefix.len() - self.prefix_cap;
                self.prefix.drain(0..drop);
            }
        }

        if loud {
            self.silence_ms = 0;
            self.speech_ms = self.speech_ms.saturating_add(fm);
            if !self.speaking && self.speech_ms >= self.cfg.min_speech_ms {
                self.speaking = true;
                self.buffer.clear();
                self.buffer.append(&mut self.prefix); // prefix pad + is now the start of the utterance
                self.buffer.extend_from_slice(frame);
                return Some(VadEvent { kind: VadEventKind::StartOfSpeech, pcm: Vec::new() });
            }
        } else {
            self.speech_ms = 0;
            self.silence_ms = self.silence_ms.saturating_add(fm);
            if self.speaking && self.silence_ms >= self.cfg.min_silence_ms {
                self.speaking = false;
                let pcm = std::mem::take(&mut self.buffer);
                self.prefix.clear();
                return Some(VadEvent { kind: VadEventKind::EndOfSpeech, pcm });
            }
        }
        None
    }
}

// ── ASR trait + stub (real sherpa-onnx plugs in here) ──────────────────────────
pub trait Asr: Send + Sync {
    /// Batch-recognize one utterance's PCM. (Streaming ASR can also emit interims; M2 starts batch.)
    fn recognize(&self, pcm: &[i16], sample_rate: u32) -> anyhow::Result<String>;
}

/// Placeholder until the real ASR (sherpa-onnx Zipformer-zh) is wired — returns empty text so the
/// audio→VAD→segment→chunk plumbing is verifiable offline.
pub struct StubAsr;
impl Asr for StubAsr {
    fn recognize(&self, _pcm: &[i16], _sample_rate: u32) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

/// VAD-gated segmenter (livekit `StreamAdapterWrapper`): frames in → SpeechEvents out. On EndOfSpeech
/// it batch-recognizes the accumulated utterance and emits a Final event.
pub struct VadSegmenter<A: Asr> {
    vad: EnergyVad,
    asr: A,
    sample_rate: u32,
}

impl<A: Asr> VadSegmenter<A> {
    pub fn new(cfg: VadConfig, asr: A) -> Self {
        let sample_rate = cfg.sample_rate;
        VadSegmenter { vad: EnergyVad::new(cfg), asr, sample_rate }
    }

    pub fn push_frame(&mut self, frame: &[i16]) -> Vec<SpeechEvent> {
        match self.vad.push_frame(frame) {
            Some(VadEvent { kind: VadEventKind::StartOfSpeech, .. }) => {
                vec![SpeechEvent { kind: SpeechEventKind::StartOfSpeech, text: None, duration_ms: 0.0 }]
            }
            Some(VadEvent { kind: VadEventKind::EndOfSpeech, pcm }) => {
                let dur = (pcm.len() as f32 / self.sample_rate as f32) * 1000.0;
                let text = self.asr.recognize(&pcm, self.sample_rate).ok().filter(|s| !s.is_empty());
                vec![SpeechEvent { kind: SpeechEventKind::Final, text, duration_ms: dur }]
            }
            None => Vec::new(),
        }
    }
}

/// Endpointing delays (livekit `voice/endpointing.py`). Streaming defaults are tighter.
#[derive(Debug, Clone, Copy)]
pub struct Endpointing {
    pub min_delay_ms: u32,
    pub max_delay_ms: u32,
}
impl Default for Endpointing {
    fn default() -> Self {
        Endpointing { min_delay_ms: 500, max_delay_ms: 3000 }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn silence(n: usize) -> Vec<i16> {
        vec![0i16; n]
    }
    fn tone(frame_idx: usize, samples: usize, sr: u32, amp: f32) -> Vec<i16> {
        (0..samples)
            .map(|k| {
                let t = (frame_idx * samples + k) as f32 / sr as f32;
                (amp * (2.0 * PI * 440.0 * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn segments_one_burst() {
        let sr = 16000u32;
        let frame = (sr / 50) as usize; // 20ms = 320 samples
        let mut seg = VadSegmenter::new(VadConfig::default(), StubAsr);

        let mut kinds: Vec<SpeechEventKind> = Vec::new();
        // 0.4s silence
        for _ in 0..20 {
            for e in seg.push_frame(&silence(frame)) { kinds.push(e.kind); }
        }
        // 0.6s tone (30 frames, amp 6000 → RMS ~4200 >> threshold)
        for i in 0..30 {
            for e in seg.push_frame(&tone(i, frame, sr, 6000.0)) { kinds.push(e.kind); }
        }
        // 0.8s silence (40 frames > min_silence 550ms) → triggers EndOfSpeech → Final
        for _ in 0..40 {
            for e in seg.push_frame(&silence(frame)) { kinds.push(e.kind); }
        }

        assert!(kinds.contains(&SpeechEventKind::StartOfSpeech), "expected StartOfSpeech, got {kinds:?}");
        assert!(kinds.contains(&SpeechEventKind::Final), "expected Final, got {kinds:?}");
    }

    #[test]
    fn pure_silence_no_events() {
        let sr = 16000u32;
        let frame = (sr / 50) as usize;
        let mut seg = VadSegmenter::new(VadConfig::default(), StubAsr);
        let mut count = 0;
        for _ in 0..100 {
            count += seg.push_frame(&silence(frame)).len();
        }
        assert_eq!(count, 0);
    }
}
