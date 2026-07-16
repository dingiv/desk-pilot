//! Batch-transcribe several WAVs through SenseVoice (loads the model once). Reads each file's real
//! sample rate via `audio_aura_asr::wav`.
//! Run: SHERPA_LIB_PATH=<dir> cargo run -p audio-aura-asr --features sherpa --example batch_transcribe -- <wav>...

use std::path::Path;
use std::time::Instant;

use audio_aura_asr::onnx::{AsrConfig, OnnxAsr};
use audio_aura_asr::{wav, Asr};

fn main() -> anyhow::Result<()> {
    let base = "/workspaces/gui_agent/audio-aura/native/models/sensevoice";
    let asr = OnnxAsr::new(AsrConfig {
        model: format!("{base}/model.int8.onnx"),
        tokens: format!("{base}/tokens.txt"),
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
