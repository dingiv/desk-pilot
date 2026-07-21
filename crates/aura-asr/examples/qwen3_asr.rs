//! Batch-transcribe WAVs through Qwen3-Audio ASR (loads the model once). Smoke test for the
//! `AsrBackend::Qwen3Asr` path — also quantifies the CPU latency caveat (autoregressive encoder-
//! decoder LLM-style; sherpa-onnx ships CPU-only libs here). Model paths resolve via the `MODELS`
//! namespace (`assets/models/qwen3-asr/`).
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

    // ASR_BACKEND: qwen3-asr (default) | sensevoice. ASR_PROVIDER: cpu (default) | cuda.
    // NUM_THREADS: onnxruntime CPU intra-op threads (default 2; this box has 16).
    let backend = std::env::var("ASR_BACKEND").unwrap_or_else(|_| "qwen3-asr".into());
    let provider = std::env::var("ASR_PROVIDER").unwrap_or_else(|_| "cpu".into());
    let nt: i32 = std::env::var("NUM_THREADS").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    eprintln!("[asr] backend={backend} provider={provider} num_threads={nt}");

    let asr = match backend.as_str() {
        "sensevoice" | "sense-voice" => OnnxAsr::new(AsrConfig {
            backend: AsrBackend::SenseVoice {
                model: p("MODELS::sensevoice/model.int8.onnx"),
                language: "auto".into(),
            },
            tokens: p("MODELS::sensevoice/tokens.txt"),
            provider,
            num_threads: nt,
            ..Default::default()
        })?,
        _ => OnnxAsr::new(AsrConfig {
            backend: AsrBackend::Qwen3Asr {
                conv_frontend: p("MODELS::qwen3-asr/conv_frontend.onnx"),
                encoder: p("MODELS::qwen3-asr/encoder.int8.onnx"),
                decoder: p("MODELS::qwen3-asr/decoder.int8.onnx"),
                tokenizer: p("MODELS::qwen3-asr/tokenizer"),
            },
            tokens: String::new(), // Qwen3 loads its vocab from the tokenizer dir
            provider,
            num_threads: nt,
            ..Default::default()
        })?,
    };

    // Default sample set: a Chinese + an English clip from the bundled testwavs.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        args = vec![
            p("MODELS::testwavs/zh-standard-0.wav"),
            p("MODELS::testwavs/en.wav"),
        ];
    }

    // Round 0 — WARMUP (discard): on GPU the first call pays one-off cuDNN benchmark + kernel
    // JIT (~tens of seconds on a fresh Blackwell/sm_120 process), and the algo picked during
    // warmup can even mis-decode. Measure steady state in round 1.
    eprintln!("[qwen3_asr] warmup round (cuDNN benchmark + JIT, results discarded) …");
    let w_t = Instant::now();
    for path in &args {
        if let Ok((pcm, sr)) = wav::read_wav_i16(Path::new(path)) {
            for _ in 0..3 {
                let _ = asr.recognize(&pcm, sr); // a few iters to fully settle cuDNN algo selection
            }
        }
    }
    eprintln!("[qwen3_asr] warmup done in {}ms\n", w_t.elapsed().as_millis());

    // Steady-state: REPEAT iters per clip (default 5); report min/p50/mean to separate true GPU
    // compute from any residual cuDNN algo churn. Set REPEAT=10 for tighter numbers.
    let repeat: usize = std::env::var("REPEAT").ok().and_then(|s| s.parse().ok()).unwrap_or(5);
    println!("{:<22} {:>3} {:>5} {:>6} {:>6} {:>6}  transcript", "file", "n", "dur", "min", "p50", "mean");
    println!("{}", "-".repeat(82));
    for path in &args {
        let name = Path::new(path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let (pcm, sr) = match wav::read_wav_i16(Path::new(path)) {
            Ok(v) => v,
            Err(e) => {
                println!("{name:<22} read error: {e}");
                continue;
            }
        };
        let dur = pcm.len() as f32 / sr as f32;
        let mut times: Vec<u64> = Vec::with_capacity(repeat);
        let mut text = String::new();
        for _ in 0..repeat {
            let t = Instant::now();
            match asr.recognize(&pcm, sr) {
                Ok(s) => {
                    text = s.trim().into();
                    times.push(t.elapsed().as_millis() as u64);
                }
                Err(e) => text = format!("err: {e}"),
            }
        }
        times.sort_unstable();
        let min = times.first().copied().unwrap_or(0);
        let p50 = times.get(times.len() / 2).copied().unwrap_or(0);
        let mean = if times.is_empty() { 0 } else { times.iter().sum::<u64>() / times.len() as u64 };
        println!("{name:<22} {repeat:>3} {dur:>4.1}s {min:>5}ms {p50:>5}ms {mean:>5}ms  {text}");
    }
    Ok(())
}
