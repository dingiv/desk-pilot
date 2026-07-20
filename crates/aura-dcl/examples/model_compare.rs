//! 对比三个模型对同一句 ASR 原文的纠错效果。
use audio_aura_router::{parse_decision, Calibrator};
use std::time::Instant;

const BASE: &str = "/workspaces/gui_agent/audio-aura/native/models";

// hungry_snake 第2句的 ASR 原文（含典型同音错误）
const TEST_INPUTS: &[(&str, &str)] = &[
    ("rost", "嗯，使用rost语言作为开发语言，图形渲染呢采用B位引擎"),
    ("蛇声", "吃到食物之后，舌声长度增加一节，同时记分起加一"),
    ("readdme", "项目结束以后，请自动生成readd me文档"),
    ("开放", "开放时间早上9点至下午5点"),
];

fn test_model(name: &str, dir: &str, file: &str, use_json_prompt: bool) {
    eprintln!("\n=== {} ===", name);
    let calibrator = match Calibrator::load(dir, file) {
        Ok(r) => r,
        Err(e) => { eprintln!("  load failed: {e}"); return; }
    };
    // warmup
    let _ = calibrator.calibrate_blocking("test", None, &[]);

    for (label, input) in TEST_INPUTS {
        let t = Instant::now();
        if use_json_prompt {
            // Qwen 用 JSON 整流+路由 prompt
            let hotwords = vec!["Bevy".to_string(), "Rust".to_string(), "蛇身".to_string(), "计分器".to_string(), "README".to_string()];
            let out = calibrator.calibrate_blocking(input, None, &hotwords).unwrap_or_default();
            let d = parse_decision(&out, input);
            let ms = t.elapsed().as_millis();
            eprintln!("  [{label}] {ms}ms");
            eprintln!("    原: {input}");
            eprintln!("    修: {}", d.calibrated_text);
        } else {
            // chinese-text-correction 模型用直接纠错 prompt
            let prompt = format!("纠错：{}", input);
            let out = calibrator.calibrate_blocking(&prompt, None, &[]).unwrap_or_default();
            let ms = t.elapsed().as_millis();
            eprintln!("  [{label}] {ms}ms");
            eprintln!("    原: {input}");
            eprintln!("    修: {out}");
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Model 1: Qwen3-1.7B Q8 (current baseline)
    test_model("Qwen3-1.7B Q8", BASE, "Qwen3-1.7B-Q8_0.gguf", true);

    // Model 2: Qwen3-4B Q4_K_M
    test_model("Qwen3-4B Q4_K_M", BASE, "Qwen3-4B-Q4_K_M.gguf", true);

    // Model 3: chinese-text-correction-1.5b Q5_K_M (专用纠错)
    test_model("CTC-1.5B Q5_K_M", BASE, "chinese-text-correction-1.5b-Q5_K_M.gguf", false);

    Ok(())
}
