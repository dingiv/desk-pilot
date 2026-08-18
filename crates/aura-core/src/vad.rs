//! vad — the OLD pure-logic VAD pieces (livekit-port era), kept for tests/examples and as a
//! zero-dependency fallback: an energy VAD with hysteresis (ported from livekit-agents Silero
//! params), a VAD-gated segmenter (the `StreamAdapter` pattern: accumulate frames between
//! speech start/end, then batch-recognize), `SpeechEvent`, and the endpointing config.
//! The PRODUCTION VAD is Silero via `dp_models::onnx` (feature `asr`, see `executor`).
//!
//! Design mirror: livekit `vad.py` (VADEvent SOS/EOS carrying accumulated frames),
//! `stt/stream_adapter.py` (VAD-gated batch→streaming), `voice/endpointing.py` (min/max
//! delay). See docs/aura/livekit-port-notes.md. (Moved from the former aura-asr crate root.)

use serde::Serialize;

use crate::{Asr, VadEvent, VadEventKind};

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

/// Config mirrors livekit Silero defaults (activation/min-durations), adapted to energy gating.
/// (Energy-gate config — NOT dp-models' Silero `VadConfig`; no relation beyond the name.)
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

/// Placeholder until a real ASR is wired — returns empty text so the audio→VAD→segment→chunk
/// plumbing is verifiable offline.
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
