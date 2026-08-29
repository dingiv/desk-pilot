//! remote — 烟测 dp-models 的 remote provider (HttpLlm/HttpAsr) 调一个 OpenAI 兼容服务。
//! 证明 local/remote 切换的 remote 侧链路通。
//!
//! 用法:
//!   - OpenAI 兼容 mock:起 `python scripts/models/serve.py mock --port 8765`,
//!     再跑 `LLM_ENDPOINT=http://127.0.0.1:8765 cargo run -p dp-models --example remote`
//!   - dp-router(支持 warm):
//!     起 `cargo run -p dp-router`(配置了 `models_root`),再跑
//!     `LLM_ENDPOINT=http://127.0.0.1:8080 LLM_MODEL=qwen2.5-3b cargo run -p dp-models --example remote`
//!     — warm() 会触发动态加载(若 model 未预加载)并等到 online 再发 chat。

use dp_models::http::{HttpAsr, HttpLlm, WarmOptions};
use dp_models::{AsrProvider, LlmProvider};

fn main() -> anyhow::Result<()> {
    let ep = std::env::var("LLM_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:8765".into());
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "test-model".into());
    eprintln!("[remote] endpoint = {ep}, model = {model}");

    let llm = HttpLlm::new(&ep, &model);

    // warm() 仅对 dp-router 有效;对 plain OpenAI mock 会返 NotDpRouter — 容错跳过。
    match llm.warm_with_options(WarmOptions {
        poll_interval_ms: 500,
        load_timeout_s: 60,
    }) {
        Ok(out) => eprintln!(
            "[warm] ready in {}ms (already_online={}, load_triggered={})",
            out.elapsed_ms, out.already_online, out.load_triggered
        ),
        Err(e) => eprintln!("[warm] skipped ({e}) — endpoint 不是 dp-router,直接调 chat"),
    }

    let resp = llm.complete("you are a test echo", "hello")?;
    println!("[llm] -> {resp}");

    let asr = HttpAsr::new(&ep);
    let pcm = vec![0i16; 16000]; // 1s silence
    let text = asr.recognize(&pcm, 16000)?;
    println!("[asr] -> {text}");

    Ok(())
}
