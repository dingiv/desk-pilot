//! audio-aura-tts — text-to-speech **capability interface** (placeholder). The text to synthesize
//! is produced upstream by the agent layer (Stage3 / desktop-pet) — e.g. a secretary reply or a
//! read-back of a calibrated utterance. This crate only defines the [`Tts`] trait + a [`NoopTts`];
//! a real backend (Kokoro / Piper via sherpa-onnx) plugs in behind the same trait later.
//!
//! This is the speech-OUTPUT counterpart to `audio-aura-asr` (speech input). It is a leaf crate.

use anyhow::Result;

/// Synthesize text → mono PCM samples the caller plays back. Implementations choose sample rate /
/// encoding; `NoopTts` produces silence.
pub trait Tts: Send + Sync {
    /// Synthesize `text` to i16 mono PCM (empty vec = silence / unavailable).
    fn synthesize(&self, text: &str) -> Result<Vec<i16>>;
    /// Sample rate of the produced PCM (default 16 kHz, matching the rest of the stack).
    fn sample_rate(&self) -> u32 {
        16_000
    }
    /// Whether this backend actually produces audio. `NoopTts` returns false so callers can skip
    /// playback instead of playing silence.
    fn is_available(&self) -> bool {
        true
    }
}

/// No-op backend: produces empty PCM. Used until a real TTS model is wired in, and as a default
/// when the user has disabled speech output.
#[derive(Default)]
pub struct NoopTts;

impl Tts for NoopTts {
    fn synthesize(&self, _text: &str) -> Result<Vec<i16>> {
        Ok(Vec::new())
    }
    fn is_available(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_is_silent_and_unavailable() {
        let tts = NoopTts;
        assert!(!tts.is_available());
        assert!(tts.synthesize("hello").unwrap().is_empty());
    }
}
