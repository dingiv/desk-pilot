//! stage12_live — thin Stage1→Stage2 bench built on `audio_aura_core::Pipeline`. (Moved here from
//! aura-asr: the old "noodle" loop now lives inside `OnnxStage1Executor` + `Pipeline`.) Streams
//! omni-scout `/audio`, runs two-pass Stage1 + Qwen calibration, and writes bench/live-*.md.
//!
//! Stage3 is NOT exercised here (this is the S1→S2 behavior benchmark). The Stage3 feedback loop
//! lives in the `daemon` crate.
//!
//! Run: cargo run -p audio-aura-core --example stage12_live --features asr,cuda -- 127.0.0.1:7879

use std::fs;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use audio_aura_asr::executor::{OnnxStage1Executor, Stage1Config};
use audio_aura_core::{Pipeline, TurnEvent};
use audio_aura_router::calibrator::Stage2CalibratorImpl;
use audio_aura_router::Calibrator;

const REPORT_DIR: &str = "/workspaces/gui_agent/audio-aura/bench";

fn cell(s: &str) -> String {
    s.replace('|', "/").replace('\n', " ")
}

fn main() -> anyhow::Result<()> {
    let scout_addr = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SCOUT_ADDR").ok())
        .unwrap_or_else(|| "127.0.0.1:7878".to_string());

    // Shared hotword store (the Stage3→Stage2 feedback channel; Stage3 is off in this bench, but
    // the store is the same shape the daemon uses).
    let hotwords: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![
        "Bevy".into(),
        "Rust".into(),
        "贪吃蛇".into(),
        "蛇身".into(),
        "计分器".into(),
        "README".into(),
    ]));

    eprintln!("[load] Stage1 (Silero VAD + 流式 Zipformer + SenseVoice) + Stage2 (Qwen3-1.7B) …");
    let s1 = OnnxStage1Executor::new(Stage1Config::new(scout_addr.clone()))?;
    let calibrator = Calibrator::load_default("Qwen3-1.7B-Q8_0.gguf")?;
    let _ = calibrator.calibrate_blocking("你好", None, &[]); // HF warmup
    let s2 = Stage2CalibratorImpl::new(Arc::new(calibrator), Arc::clone(&hotwords));

    fs::create_dir_all(REPORT_DIR).ok();
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let report = Arc::new(Mutex::new(fs::File::create(format!("{REPORT_DIR}/live-{epoch}.md"))?));
    writeln!(
        report.lock().unwrap(),
        "# Stage1→Stage2 (Pipeline) · {epoch}\n\n\
         - 源: omni-scout `{scout_addr}/audio`\n\n\
         | # | 时刻(s) | 路由(ms) | 意图 | 流式(热词) | 批式原文 | Stage2整流 | 回应 |\n\
         |---|---:|---:|---|---|---|---|---|"
    )?;

    println!("\n● Pipeline 就绪 (scout {scout_addr}/audio). Ctrl-C 结束.\n");
    Pipeline::new(s1, Box::new(s2)).run(move |ev| match ev {
        TurnEvent::Interim { seq: _, partial, at_s } => println!("  …流式 @{at_s:.1}s: {partial}"),
        TurnEvent::Final { utterance: u, decision: d, route_ms } => {
            println!(
                "▶ #{} @{:.1}s ({}s) [{}] 路由 {:.0}ms\n   流式: {}\n   原文: {}\n   整流: {}\n   回应: {}\n",
                u.seq, u.at_s, u.duration_ms / 1000.0, d.intent, route_ms,
                u.streaming_text, u.raw_text, d.calibrated_text, d.reply
            );
            let _ = writeln!(
                report.lock().unwrap(),
                "| {} | {:.1} | {:.0} | {} | {} | {} | {} | {} |",
                u.seq, u.at_s, route_ms, d.intent,
                cell(&u.streaming_text), cell(&u.raw_text),
                cell(&d.calibrated_text), cell(&d.reply)
            );
        }
    });
}
