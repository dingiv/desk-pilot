//! stage12_live — thin Stage1→Stage2 bench built on `audio_aura_core::Pipeline`. (Moved here from
//! aura-asr: the old "noodle" loop now lives inside `OnnxStage1Recognizer` + `Pipeline`.) Streams
//! omni-scout `/audio`, runs two-pass Stage1 + Stage2 calibration (over dp-router), and writes
//! bench/live-*.md.
//!
//! Stage3 is NOT exercised here (this is the S1→S2 behavior benchmark). The Stage3 feedback loop
//! lives in the `daemon` crate.
//!
//! Run: cargo run -p audio-aura-core --example stage12_live --features asr -- 127.0.0.1:7879
//!
//! dp-router endpoint / model 可由 DP_ROUTER_ENDPOINT / DP_ROUTER_MODEL 覆盖(默认
//! http://127.0.0.1:8080 / qwen2.5-3b-instruct-q4_k_m)。

use std::fs;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use audio_aura_core::recognizer::{OnnxStage1Recognizer, Stage1Config};
use audio_aura_core::{Calibrator, LlmInput, Pipeline, Stage2CalibratorImpl, TurnEvent};

// Repo-relative bench dir (crates/aura-core → desk-pilot/bench). Created on startup.
const REPORT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench");

fn cell(s: &str) -> String {
    s.replace('|', "/").replace('\n', " ")
}

fn main() -> anyhow::Result<()> {
    let scout_addr = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SCOUT_ADDR").ok())
        .unwrap_or_else(|| "127.0.0.1:7878".to_string());

    let router_endpoint = std::env::var("DP_ROUTER_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let router_model = std::env::var("DP_ROUTER_MODEL")
        .unwrap_or_else(|_| "qwen2.5-3b-instruct-q4_k_m".to_string());

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

    eprintln!(
        "[load] Stage1 (Silero VAD + 流式 Zipformer + SenseVoice) + Stage2 ({router_model} via {router_endpoint}) …"
    );
    // round12 异步化:batch 由 Pipeline 的 per-paragraph 任务自建(batch_jobs=false),
    // s1 不再投 job —— batch_rx 直接丢弃。
    let (s1, _batch_rx) = OnnxStage1Recognizer::new(Stage1Config::new(scout_addr.clone()))?;
    let calibrator = Calibrator::load(&router_endpoint, &router_model)?;
    let _ = calibrator.calibrate_blocking("你好", None, &[]); // HTTP warmup (避免首轮冷启动)
    let corrections = Arc::new(Mutex::new(Vec::new()));
    let s2 = Stage2CalibratorImpl::new(Arc::new(calibrator), Arc::clone(&hotwords), corrections, LlmInput::Batch);

    fs::create_dir_all(REPORT_DIR).ok();
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let report = Arc::new(Mutex::new(fs::File::create(format!("{REPORT_DIR}/live-{epoch}.md"))?));
    writeln!(
        report.lock().unwrap(),
        "# Stage1→Stage2 (Pipeline · 边界范式 · batch 异步) · {epoch}\n\n\
         - 源: omni-scout `{scout_addr}/audio`\n\n\
         | 段落 | 定稿路由(ms) | Stage2整流(定稿) |\n\
         |---|---:|---|"
    )?;

    println!("\n● Pipeline 就绪 (scout {scout_addr}/audio). Ctrl-C 结束.\n");
    Pipeline::new(s1, Box::new(s2)).run(
        Arc::new(AtomicBool::new(true)),
        Arc::new((Mutex::new(()), Condvar::new())),
        move |ev| match ev {
        TurnEvent::ParagraphClosed { paragraph_id } => {
            println!("  ●段关闭 w{paragraph_id}(文本定格,定稿修订稍后)")
        }
        TurnEvent::StreamFragment { paragraph_id, sentence_id: _, text, at_s } => {
            println!("  …流式 w{paragraph_id} @{at_s:.1}s: {text}")
        }
        TurnEvent::BatchSentence { paragraph_id, sentence_id, text } => {
            println!("  ≈ 句批 w{paragraph_id} s{sentence_id}: {text}")
        }
        TurnEvent::BatchParagraph { paragraph_id, text } => {
            println!("  ≈ 段批 w{paragraph_id}: {text}")
        }
        TurnEvent::SentenceCalibration { paragraph_id, calibrated, route_ms } => {
            println!("  ≈ w{paragraph_id} 整流中 @{route_ms:.0}ms: {calibrated}");
        }
        TurnEvent::ParagraphCalibration { paragraph_id, calibrated, route_ms } => {
            println!("\n▶ 定稿 w{paragraph_id} 路由 {route_ms:.0}ms: {calibrated}\n");
            let _ = writeln!(
                report.lock().unwrap(),
                "| {} | {:.0} | {} |",
                paragraph_id, route_ms, cell(&calibrated)
            );
        }
    });
}
