//! Test streaming ASR: feed zh.wav chunk-by-chunk, print partial results at intervals to see
//! the "phone input method" correction effect (earlier text changes as more audio arrives).
//!
//! Run: cargo run -p audio-aura-core --example streaming_asr -- [wav_path]

use std::path::Path;
use std::time::Instant;

use dp_models::onnx::{OnlineAsr, StreamingAsrConfig};
use audio_aura_core::wav;

fn main() -> anyhow::Result<()> {
    let fs = shared::loader!();
    let p = |rel: &str| fs.resolve(rel).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let base = p("MODELS::zipformer-streaming-zh-en");
    let wav_path = std::env::args().nth(1).unwrap_or_else(|| p("MODELS::sensevoice/test_wavs/zh.wav"));

    eprintln!("[load] streaming Zipformer (with hotwords) …");
    let t = Instant::now();
    let asr = OnlineAsr::new(StreamingAsrConfig {
        encoder: format!("{base}/encoder-epoch-99-avg-1.onnx"),
        decoder: format!("{base}/decoder-epoch-99-avg-1.onnx"),
        joiner: format!("{base}/joiner-epoch-99-avg-1.onnx"),
        tokens: format!("{base}/tokens.txt"),
        bpe_vocab: format!("{base}/bpe.vocab"),
        hotwords: vec!["Bevy".into(), "Rust".into(), "贪吃蛇".into(), "蛇身".into(), "计分器".into(), "README".into()],
        hotwords_score: 2.0,
        ..Default::default()
    })?;
    eprintln!("[load] ready in {:.2}s", t.elapsed().as_secs_f64());

    let (pcm, sr) = wav::read_wav_i16(Path::new(&wav_path))?;
    eprintln!("[wav] {} samples ({:.1}s @{sr})", pcm.len(), pcm.len() as f32 / sr as f32);

    let session = asr.create_session();
    let chunk = (sr / 10) as usize; // 100ms chunks
    let total = pcm.len();

    eprintln!("[stream] feeding in 100ms chunks, showing partial every ~500ms:\n");
    let mut last_print = 0usize;
    for (i, frame) in pcm.chunks(chunk).enumerate() {
        session.accept_waveform(sr as i32, frame);
        let pos = i * chunk;
        let secs = pos as f32 / sr as f32;

        // Print partial every ~500ms to observe correction
        if pos - last_print >= (sr as usize) / 2 {
            let text = asr.decode_and_result(&session);
            if !text.is_empty() {
                eprintln!("  @{secs:.1}s: {text}");
            }
            last_print = pos;
        }
    }

    session.input_finished();
    let final_text = asr.decode_and_result(&session);
    let elapsed = t.elapsed().as_secs_f64();
    eprintln!("\n[final] {final_text}");
    eprintln!("[done] {elapsed:.2}s total (audio {:.1}s)", total as f32 / sr as f32);
    Ok(())
}
