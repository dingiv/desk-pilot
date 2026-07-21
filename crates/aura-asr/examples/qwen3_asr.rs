//! Batch-transcribe WAVs through Qwen3-Audio ASR (loads the model once). Smoke test for the
//! `AsrBackend::Qwen3Asr` path — also quantifies the CPU latency caveat (autoregressive encoder-
//! decoder LLM-style; sherpa-onnx ships CPU-only libs here). Model paths resolve via the `MODELS`
//! namespace (`native/models/qwen3-asr/`).
//!
//! Run:
//!   cargo run -p audio-aura-asr --features onnx --example qwen3_asr -- [wav]...
//! (no args → transcribes the bundled `testwavs/zh-standard-0.wav` + `en.wav`)

use std::path::Path;
use std::time::Instant;

use audio_aura_asr::onnx::{AsrBackend, AsrConfig, OnnxAsr};
use audio_aura_asr::Asr;
use audio_aura_store::wav;

fn main() -> anyhow::Result<()> {
    let fs = shared::loader!();
    let p = |rel: &str| fs.resolve(rel).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();

    let asr = OnnxAsr::new(AsrConfig {
        backend: AsrBackend::Qwen3Asr {
            conv_frontend: p("MODELS::qwen3-asr/conv_frontend.onnx"),
            encoder: p("MODELS::qwen3-asr/encoder.int8.onnx"),
            decoder: p("MODELS::qwen3-asr/decoder.int8.onnx"),
            tokenizer: p("MODELS::qwen3-asr/tokenizer"),
        },
        tokens: String::new(), // Qwen3 loads its vocab from the tokenizer dir
        ..Default::default()
    })?;

    // Default sample set: a Chinese + an English clip from the bundled testwavs.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        args = vec![
            p("MODELS::testwavs/zh-standard-0.wav"),
            p("MODELS::testwavs/en.wav"),
        ];
    }

    println!("{:<24} {:>7} {:>6} {:>9}  transcript", "file", "rate", "dur", "asr");
    println!("{}", "-".repeat(80));
    for path in &args {
        let name = Path::new(path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let (pcm, sr) = match wav::read_wav_i16(Path::new(path)) {
            Ok(v) => v,
            Err(e) => {
                println!("{name:<24} read error: {e}");
                continue;
            }
        };
        let dur = pcm.len() as f32 / sr as f32;
        let t = Instant::now();
        match asr.recognize(&pcm, sr) {
            Ok(text) => println!(
                "{name:<24} {sr:>6}Hz {dur:>5.1}s {:>8}ms  {}",
                t.elapsed().as_millis(),
                text.trim()
            ),
            Err(e) => println!("{name:<24} recognize error: {e}"),
        }
    }
    Ok(())
}
