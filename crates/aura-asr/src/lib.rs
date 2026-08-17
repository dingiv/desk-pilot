//! audio-aura-asr — Stage1: audio ingest (scout→ring), the **boundary-paradigm data contract**
//! ([`VadSegment`]/[`VadWindow`]/[`Stage1Event`], 2026-08-17 重构 — see
//! docs/aura/vad-segment-model.md), the PCM [`audio_store::AudioStore`], and the ONNX executor
//! (`executor`, feature-gated: Silero VAD + per-segment streaming sessions + per-segment batch +
//! window settle). This crate also keeps the older pure-logic VAD pieces (energy VAD,
//! `VadSegmenter`, `SpeechEvent` — livekit-port era) used by tests/examples.
//!
//! Design mirror: livekit `vad.py` (VADEvent SOS/EOS carrying accumulated frames),
//! `stt/stream_adapter.py` (VAD-gated batch→streaming), `voice/endpointing.py` (min/max delay).
//! See docs/aura/livekit-port-notes.md.

use serde::Serialize;

pub mod audio_store;
pub mod buffer;
pub mod scout;
pub mod source;

/// Stage1 stage-boundary abstractions: `Stage1Executor` (capture+VAD+two-pass ASR → events) +
/// the `Utterance`/`Stage1Event` data contract. The data types are always compiled (so the
/// Stage2 crate can reference `Utterance` without enabling the heavy `onnx` feature); the
/// executor impl + its config are `onnx`-gated.
#[cfg(feature = "onnx")]
pub mod executor;

// ── ONNX 语音栈已迁至 dp-models ─────────────────────────────────────────
// VAD (Silero) + 流式 ASR (Zipformer) + batch ASR (SenseVoice/…) 的 sherpa-onnx 封装
// 在 `dp_models::onnx`(feature `speech`)。本 crate 的 `onnx` feature 转发开启它:
// audio-aura 不再直接依赖 sherpa-onnx。VAD 数据契约经 dp_models re-export。

// ── Stage1 → Stage2 data contract · 边界范式（VadSegment / VadWindow）──────────────
// 设计: docs/aura/vad-segment-model.md（2026-08-17 重构,替代旧的 Utterance/Stage1Action
// "就地修改"契约）。两个时间参数切出两级实体:
//   · VAD 间隔 (vad.min_silence)  → VadSegment  原子录音片段(段级流式会话 + 段级 batch)
//   · merge 窗口 (vad.merge_gap)  → VadWindow   多段组合(定稿单位,拼接 PCM 重跑 batch)
// PCM 由 [`audio_store::AudioStore`] 按 id 持有,实体只持 id——录音数据不随事件克隆。
// 事件 append-only + 边界标记: Batch(每段)驱动 Stage2 联合整流当前窗口,WindowEdge
// (窗口关闭)驱动定稿。batch 失败显式建模为 `Option`(远程网络可能出问题)。

/// Audio clip id — assigned by [`audio_store::AudioStore`]. Entities hold ids, never PCM.
pub type AudioId = u64;
/// Segment id — monotonic within a pipeline run.
pub type SegmentId = u64;
/// Window id — monotonic within a run, assigned when the window OPENS (its first SOS), so
/// live `Interim` partials can carry the real id (no prospective guessing).
pub type WindowId = u64;

/// One VAD-gap-delimited clip — the atomic Stage1 unit. A segment is complete the moment its
/// EOS fires: streaming session finalized, PCM inserted into the AudioStore, one batch pass
/// packed in. `batch_text: None` is LEGAL — batch depends on the remote network and may fail;
/// consumers fall back to `streaming_text` via [`VadSegment::best_text`].
#[derive(Debug, Clone)]
pub struct VadSegment {
    pub id: SegmentId,
    /// The clip's PCM, owned by the [`audio_store::AudioStore`] — never cloned into events.
    pub audio_id: AudioId,
    /// Wall-clock seconds since executor start (SOS).
    pub start_s: f64,
    /// Wall-clock seconds since executor start (EOS).
    pub end_s: f64,
    /// Per-segment streaming ASR final (hotword-biased; the session spans exactly this segment).
    pub streaming_text: String,
    /// Per-segment batch ASR result. `None` when the batch pass failed (network error) or
    /// returned empty text — HttpAsr's `Err` and OnnxAsr's empty string map to the same None.
    pub batch_text: Option<String>,
}

impl VadSegment {
    /// Best available text: `batch_text` when Some(non-empty), else `streaming_text`.
    pub fn best_text(&self) -> &str {
        self.batch_text
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&self.streaming_text)
    }
}

/// A merge-window composition of [`VadSegment`]s — the settle/final unit. Built when a big
/// gap (≥ `merge_gap_s`) or the settle-timeout closes the window; carries a snapshot of its
/// segments plus the window-level aggregation:
/// - `streaming_text` = concat of the segments' streaming finals (zero cost, no re-run);
/// - `batch_text` = ONE re-run of the batch model over the concatenated PCM (cross-segment
///   context; the authoritative text Stage2 finalizes on). `None` on a failed re-run.
#[derive(Debug, Clone)]
pub struct VadWindow {
    pub id: WindowId,
    /// Settle-time snapshot (ids/timestamps/texts only — no PCM per segment).
    pub segments: Vec<VadSegment>,
    /// SOS of the FIRST segment.
    pub start_s: f64,
    /// EOS of the LAST segment.
    pub end_s: f64,
    pub streaming_text: String,
    pub batch_text: Option<String>,
    /// The whole window's concatenated PCM — assembled once at settle, shared (Arc) between
    /// the window-level batch pass and downstream archival. The AudioStore evicts the
    /// per-segment clips right after; this Arc is the only remaining copy.
    pub pcm: std::sync::Arc<Vec<i16>>,
}

impl VadWindow {
    /// The authoritative text Stage2 finalizes on: the window-level batch re-run when present,
    /// else the concat of the segments' own best texts (per-segment batches may have succeeded
    /// even when the window re-run failed).
    pub fn best_text(&self) -> std::borrow::Cow<'_, str> {
        if let Some(t) = self.batch_text.as_deref().filter(|t| !t.trim().is_empty()) {
            return std::borrow::Cow::Borrowed(t);
        }
        std::borrow::Cow::Owned(
            self.segments.iter().map(|s| s.best_text()).collect::<Vec<_>>().join(""),
        )
    }

    /// Window duration in milliseconds (from the PCM the batch actually heard).
    pub fn duration_ms(&self) -> f32 {
        self.pcm.len() as f32 / 16_000.0 * 1000.0
    }
}

/// Events emitted by [`executor::Stage1Executor`]. Defined here (ungated) so downstream
/// crates can match on them without the `onnx` feature. Append-only — consumers never mutate
/// an earlier entity in place (the old paradigm's same-seq update is gone).
#[derive(Debug, Clone)]
pub enum Stage1Event {
    /// Live streaming partial for the CURRENT segment (per-segment session ⇒ the partial
    /// belongs to exactly one segment). Carries the real `window_id` (assigned at the
    /// window's first SOS) + `segment_id`. Passes straight through to the UI — NOT a Stage2
    /// input (D2: no live-partial calibration).
    Interim { window_id: WindowId, segment_id: SegmentId, partial: String, at_s: f64 },
    /// A VAD gap closed a segment: its batch pass is packed in. `segments` is ALL segments
    /// of the current window so far (Stage2 jointly calibrates them — the payload IS the
    /// window, keeping Stage2 stateless). Provisional until the `WindowEdge`.
    Batch { window_id: WindowId, segments: Vec<VadSegment> },
    /// The merge window closed (big gap or settle-timeout): the window-level batch re-run is
    /// done and packed. Authoritative — Stage2 finalizes on it; the AudioStore evicts the
    /// segment clips right after this event.
    WindowEdge { window: VadWindow },
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
// 数据契约定义在 dp-models(与 sherpa 的 OnnxVad 共用),这里 re-export。
pub use dp_models::{VadEvent, VadEventKind};

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

// ── ASR trait (re-export from dp-models, the cross-subsystem provider abstraction) ──
// OnnxAsr / StubAsr impl `Asr` (= dp_models::AsrProvider); a remote HttpAsr impls it too.
// VadSegmenter<A: Asr> is unchanged — `Asr` is just an alias now.
pub use dp_models::AsrProvider as Asr;

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
