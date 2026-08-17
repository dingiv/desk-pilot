//! Batch-transcribe several WAVs through SenseVoice (loads the model once). Reads each file's real
//! sample rate via `audio_aura_core::wav`. Model paths resolve via the `MODELS` namespace.
//! Run: cargo run -p audio-aura-asr --features onnx --example batch_transcribe -- <wav>...

use std::path::Path;
use std::time::Instant;

use dp_models::onnx::{AsrBackend, AsrConfig, OnnxAsr};
use audio_aura_asr::Asr;
use audio_aura_core::wav;

fn main() -> anyhow::Result<()> {
    let fs = shared::loader!();
    let p = |rel: &str| fs.resolve(rel).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();

    let asr = OnnxAsr::new(AsrConfig {
        backend: AsrBackend::SenseVoice {
            model: p("MODELS::sensevoice/model.int8.onnx"),
            language: "auto".into(),
        },
        tokens: p("MODELS::sensevoice/tokens.txt"),
        ..Default::default()
    })?;
    println!("{:<24} {:>7} {:>6} {:>7}  transcript", "file", "rate", "dur", "asr");
    println!("{}", "-".repeat(80));
    for path in std::env::args().skip(1) {
        let name = Path::new(&path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let (pcm, sr) = match wav::read_wav_i16(Path::new(&path)) {
            Ok(v) => v,
            Err(e) => { println!("{name:<24} read error: {e}"); continue; }
        };
        let dur = pcm.len() as f32 / sr as f32;
        let t = Instant::now();
        let text = asr.recognize(&pcm, sr)?;
        println!("{name:<24} {sr:>6}Hz {dur:>5.1}s {:>6}ms  {}", t.elapsed().as_millis(), text.trim());
    }
    Ok(())
}
