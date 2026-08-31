//! calibrate_bench — 测试 Stage2 纠偏模型对各种 ASR 错误文本的整流能力(走 dp-router)。
//! 每个用例模拟真实的 ASR 输出(同音错字、无标点、英文混入等)，看 Stage2 能否纠正。
//!
//! Run:
//!   cargo run -p audio-aura-core --example calibrate_bench
//!
//! 通过环境变量覆盖默认 endpoint / model:
//!   DP_ROUTER_ENDPOINT (默认 http://127.0.0.1:8080)
//!   DP_ROUTER_MODEL    (默认 qwen2.5-3b-instruct-q4_k_m)
//!
//! 可选 RUST_LOG=stage2::prompt=debug 看每轮完整提示词。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use audio_aura_core::{Calibrator, PromptBuilder};

fn main() -> anyhow::Result<()> {
    shared::init_tracing();

    // ── 连 dp-router ──
    let endpoint = std::env::var("DP_ROUTER_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let model = std::env::var("DP_ROUTER_MODEL")
        .unwrap_or_else(|_| "qwen2.5-3b-instruct-q4_k_m".to_string());
    eprintln!("[load] {model} via {endpoint} …");
    let calibrator = Calibrator::load(&endpoint, &model)?;
    let _ = calibrator.infer("system", "hi"); // warmup
    eprintln!("[load] ready\n");

    // ── 测试用例 ──
    // (名称, ASR 原文, 期望/参考结果, 热词)
    let cases: Vec<(&str, &str, &str, Vec<&str>)> = vec![
        // 1. 无标点的中文长句（来自用户真麦）
        (
            "无标点中文长句",
            "现在我们来看一下这个问题我们先将工作区中的meeting会议文件整理成markdown和PDF两种格式",
            "现在我们来看一下这个问题。我们先将工作区中的 meeting 会议文件整理成 markdown 和 PDF 两种格式。",
            vec![],
        ),
        // 2. 同音错字（经典）
        (
            "同音错字-计分器",
            "帮我用rost写一个贪吃蛇游戏加上计分起",
            "帮我用 Rust 写一个贪吃蛇游戏，加上计分器。",
            vec!["Rust"],
        ),
        // 3. 英文专有名词音译
        (
            "英文音译-Bevy",
            "采用位引擎渲染游戏画面",
            "采用 Bevy 引擎渲染游戏画面。",
            vec!["Bevy"],
        ),
        // 4. 蛇声→蛇身
        (
            "同音错字-蛇身",
            "那个蛇声长度需要增加一节",
            "那个蛇身长度需要增加一节。",
            vec![],
        ),
        // 5. 口语化 + 语气词
        (
            "口语化去语气词",
            "嗯那个就是我想说的是呢这个这个功能它其实是比较简单的对吧",
            "我想说的是，这个功能其实比较简单。",
            vec![],
        ),
        // 6. 英文 ASR 错误（大写/连读）
        (
            "英文连读",
            "the tribal chief then called for the boy and presented him with fifty pieces of gold",
            "The tribal chieftain called for the boy and presented him with fifty pieces of gold.",
            vec![],
        ),
        // 7. 中英混排无标点
        (
            "中英混排无标点",
            "首先呢打开Docker然后呢运行docker compose up这样呢就可以启动服务了",
            "首先打开 Docker，然后运行 docker compose up，这样就可以启动服务了。",
            vec!["Docker"],
        ),
        // 8. 短句（边界情况）
        (
            "短句",
            "你好",
            "你好。",
            vec![],
        ),
        // 9. 重复词
        (
            "重复词",
            "那个那个我们可以可以看一下这个问题问题",
            "我们可以看一下这个问题。",
            vec![],
        ),
        // 10. 用户纠偏测试（模拟 correction 注入后的效果）
        (
            "纠偏注入-B尾引擎",
            "这个项目用了B尾引擎",
            "这个项目用了 Bevy 引擎。",
            vec!["Bevy"],
        ),
    ];

    // ── 运行测试 ──
    println!(
        "{:<20} {:>6} {:>6}  {:<50} {:<50}",
        "用例", "ms", "判定", "ASR输入", "Stage2输出"
    );
    println!("{}", "─".repeat(140));

    let corrections: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    for (name, asr_text, expected, hotwords) in &cases {
        // 专测：case 10 模拟用户纠偏注入
        if *name == "纠偏注入-B尾引擎" {
            corrections.lock().unwrap().push(("B尾引擎".into(), "Bevy 引擎".into()));
        }

        let hw: Vec<String> = hotwords.iter().map(|s| s.to_string()).collect();
        let mut pb = PromptBuilder::new(asr_text).hotwords(&hw);
        let corr = corrections.lock().unwrap().clone();
        if !corr.is_empty() {
            pb = pb.corrections(&corr);
        }
        let (system, user) = pb.build();

        let t = Instant::now();
        let result = calibrator.infer(&system, &user).unwrap_or_default();
        let ms = t.elapsed().as_millis();

        let trimmed = result.trim();
        let pass = trimmed.contains(expected) || expected.contains(trimmed);
        let mark = if pass { "✅" } else { "❌" };

        println!(
            "{:<20} {:>4}ms {:>4}  {:<50} {:<50}",
            name,
            ms,
            mark,
            truncate(asr_text, 48),
            truncate(trimmed, 48),
        );
        if !pass {
            println!("  └ 期望: {expected}");
        }
    }

    // ── 专项：纠偏效果验证 ──
    println!("\n=== 纠偏效果专项 ===");
    println!("已注入纠偏: B尾引擎 → Bevy 引擎\n");

    let test_inputs = vec![
        "那个B尾引擎的性能不错",
        "我用B尾引擎写了个游戏",
        "B尾引擎和rost哪个好",
    ];
    for input in &test_inputs {
        let hw = vec!["Rust".to_string(), "Bevy".to_string()];
        let corr = corrections.lock().unwrap().clone();
        let (system, user) = PromptBuilder::new(input)
            .hotwords(&hw)
            .corrections(&corr)
            .build();
        let result = calibrator.infer(&system, &user).unwrap_or_default();
        let has_bevy = result.trim().to_lowercase().contains("bevy");
        println!(
            "  {} → {} {}",
            input,
            truncate(result.trim(), 40),
            if has_bevy { "✅ Bevy" } else { "❌ 未纠正" },
        );
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}
