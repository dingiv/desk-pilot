//! remote — 烟测 dp-models 的 remote provider (HttpLlm/HttpAsr) 调一个 OpenAI 兼容服务。
//! 证明 local/remote 切换的 remote 侧链路通。
//!
//! 先起 mock:  python scripts/models/serve.py mock --port 8765
//! 再跑:       LLM_ENDPOINT=http://127.0.0.1:8765 cargo run -p dp-models --example remote

use dp_models::http::{HttpAsr, HttpLlm};
use dp_models::{AsrProvider, LlmProvider};

fn main() -> anyhow::Result<()> {
    let ep = std::env::var("LLM_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:8765".into());
    eprintln!("[remote] endpoint = {ep}");

    let llm = HttpLlm::new(&ep, "test-model");
    let resp = llm.complete("you are a test echo", "hello")?;
    println!("[llm] -> {resp}");

    let asr = HttpAsr::new(&ep);
    let pcm = vec![0i16; 16000]; // 1s silence
    let text = asr.recognize(&pcm, 16000)?;
    println!("[asr] -> {text}");

    Ok(())
}
