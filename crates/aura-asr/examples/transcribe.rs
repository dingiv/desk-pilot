//! Verify real Stage1 ASR end-to-end: load SenseVoice (sherpa-onnx) and transcribe a 16 kHz mono WAV.
//! Model paths resolve via the `MODELS` namespace (`assets/models/sensevoice/`).
//! Run: cargo run -p audio-aura-asr --features onnx --example transcribe [-- <wav>]

use std::path::Path;

use audio_aura_asr::onnx::{AsrBackend, AsrConfig, OnnxAsr};
use audio_aura_asr::Asr;
use audio_aura_core::wav;

fn main() -> anyhow::Result<()> {
    let fs = shared::loader!();
    let p = |rel: &str| fs.resolve(rel).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();

    let wav_path = std::env::args().nth(1).unwrap_or_else(|| p("MODELS::sensevoice/test_wavs/zh.wav"));
    let (pcm, sr) = wav::read_wav_i16(Path::new(&wav_path))?;
    eprintln!("[wav] {} samples ({:.2}s @{sr})", pcm.len(), pcm.len() as f32 / sr as f32);

    let t0 = std::time::Instant::now();
    let asr = OnnxAsr::new(AsrConfig {
        backend: AsrBackend::SenseVoice {
            model: p("MODELS::sensevoice/model.int8.onnx"),
            language: "auto".into(),
        },
        tokens: p("MODELS::sensevoice/tokens.txt"),
        ..Default::default()
    })?;
    eprintln!("[load] SenseVoice ready in {:?}", t0.elapsed());

    let t = std::time::Instant::now();
    let text = asr.recognize(&pcm, sr)?;
    eprintln!("[asr] {:?}\n--- transcript:\n{text}", t.elapsed());
    Ok(())
}
