//! Voxtral-ASR spike — 跑 mistral.rs (candle GPU) 的 Voxtral-Mini-4B-Realtime，测延迟，对比
//! onnx Qwen3-ASR CPU (862ms) 基线。验证「candle GPU 对音频 ASR decoder 的收益」。
//!
//! 注意：mistral.rs 0.8.1 的 speculative decoding 是文本专用，**Voxtral(多模态)用不了 spec
//! decoding**——所以这里只测 candle GPU + ISQ，不带 spec。
//!
//! Run:
//!   cargo run -p audio-aura-router --features cuda --example voxtral_asr -- [wav]
//!   REPEAT=5 VOXTRAL_MODEL=native/models/voxtral-mini-4b-realtime cargo run ...

use anyhow::Result;
use mistralrs::{AudioInput, MultimodalMessages, MultimodalModelBuilder, TextMessageRole};
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<()> {
    let model_dir = std::env::var("VOXTRAL_MODEL")
        .unwrap_or_else(|_| "native/models/voxtral-mini-4b-realtime".into());
    let wav = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "native/models/testwavs/zh-standard-0.wav".into());
    let repeat: usize = std::env::var("REPEAT").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    eprintln!("[voxtral] model={model_dir} wav={wav} repeat={repeat}");

    let isq = std::env::var("ISQ").unwrap_or_default();
    let mut builder = MultimodalModelBuilder::new(&model_dir).with_logging();
    match isq.as_str() {
        "q4" | "Q4" => {
            builder = builder.with_auto_isq(mistralrs::IsqBits::Four);
            eprintln!("[voxtral] ISQ = auto Q4");
        }
        "q8" | "Q8" => {
            builder = builder.with_auto_isq(mistralrs::IsqBits::Eight);
            eprintln!("[voxtral] ISQ = auto Q8");
        }
        _ => eprintln!("[voxtral] ISQ = none (BF16)"),
    }
    let t = Instant::now();
    let model = builder.build().await?;
    eprintln!("[voxtral] loaded in {}ms", t.elapsed().as_millis());

    let audio_bytes = std::fs::read(&wav)?;

    // i=0 warmup (cudnn/JIT/conv benchmark discarded), i=1..=repeat measured.
    let mut times: Vec<u64> = Vec::new();
    for i in 0..=repeat {
        let audio = AudioInput::from_bytes(&audio_bytes)?;
        let msgs = MultimodalMessages::new().add_multimodal_message(
            TextMessageRole::User,
            "Transcribe this audio.",
            vec![],
            vec![audio],
            vec![],
        );
        let t = Instant::now();
        let resp = model.send_chat_request(msgs).await?;
        let ms = t.elapsed().as_millis() as u64;
        let text = resp.choices[0].message.content.as_ref().map(|c| c.to_string()).unwrap_or_default();
        if i == 0 {
            eprintln!("[voxtral] warmup {ms}ms (discarded): {text}");
        } else {
            times.push(ms);
            eprintln!("[voxtral] {ms}ms: {text}");
        }
    }

    times.sort_unstable();
    let n = times.len();
    eprintln!(
        "[voxtral] steady: min={}ms p50={}ms mean={}ms",
        times.first().copied().unwrap_or(0),
        times.get(n / 2).copied().unwrap_or(0),
        if n == 0 { 0 } else { times.iter().sum::<u64>() / n as u64 },
    );
    Ok(())
}
