//! dp-models — desk-pilot 的本地模型 Provider 抽象层。
//!
//! 定义统一的 task trait (`AsrProvider`/`LlmProvider`/`VlmProvider`)，下游 (aura / visual-rover)
//! 通过 trait 对象调用，不关心推理是 **local** (lib 嵌入: sherpa/mistral.rs/candle) 还是
//! **remote** (OpenAI 兼容 HTTP: vLLM/SGLang/qwen3-asr-rs server/云端)。
//!
//! 本 crate 只做抽象 + remote 实现；local 实现留在各专业 crate (OnnxAsr / Calibrator / 未来
//! candle VLM)，它们 `impl dp_models::XxxProvider`。工厂 (选 local/remote) 在各 app
//! (aura-daemon / visual-rover-app)。
//!
//! 所有 trait **同步** (匹配 Stage1 的同步线程模型；remote 实现用 `reqwest::blocking`)。

pub mod config;
pub mod http;

pub use config::ProviderKind;

/// 语音转文字 (ASR): 输入 PCM i16 mono, 返回转写文本。
pub trait AsrProvider: Send + Sync {
    fn recognize(&self, pcm: &[i16], sample_rate: u32) -> anyhow::Result<String>;
}

/// 文本 LLM (如 Stage2 整流/路由): (system, user) -> 文本。
pub trait LlmProvider: Send + Sync {
    fn complete(&self, system: &str, user: &str) -> anyhow::Result<String>;
}

/// 视觉语言模型 (VLM): (system, user, image_png) -> 文本。local 实现留 visual-rover 未来。
pub trait VlmProvider: Send + Sync {
    fn complete(&self, system: &str, user: &str, image_png: &[u8]) -> anyhow::Result<String>;
}
