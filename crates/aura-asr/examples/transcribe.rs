//! Verify real Stage1 ASR end-to-end: load SenseVoice (sherpa-onnx) and transcribe a 16 kHz mono WAV.
//! Run: SHERPA_LIB_PATH=<sherpa dir> cargo run -p audio-aura-asr --features sherpa --example transcribe [-- <wav>]

use std::path::Path;

use audio_aura_asr::onnx::{AsrConfig, OnnxAsr};
use audio_aura_asr::Asr;
use audio_aura_store::wav;

fn main() -> anyhow::Result<()> {
    let base = "/workspaces/gui_agent/audio-aura/native/models/sensevoice";
    let wav_path = std::env::args().nth(1).unwrap_or_else(|| format!("{base}/test_wavs/zh.wav"));
    let (pcm, sr) = wav::read_wav_i16(Path::new(&wav_path))?;
    eprintln!("[wav] {} samples ({:.2}s @{sr})", pcm.len(), pcm.len() as f32 / sr as f32);

    let t0 = std::time::Instant::now();
    let asr = OnnxAsr::new(AsrConfig {
        model: format!("{base}/model.int8.onnx"),
        tokens: format!("{base}/tokens.txt"),
        ..Default::default()
    })?;
    eprintln!("[load] SenseVoice ready in {:?}", t0.elapsed());

    let t = std::time::Instant::now();
    let text = asr.recognize(&pcm, sr)?;
    eprintln!("[asr] {:?}\n--- transcript:\n{text}", t.elapsed());
    Ok(())
}
