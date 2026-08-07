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

/// ONNX 实时语音栈(feature `speech`):VAD (Silero) + 流式 ASR (Zipformer) +
/// batch ASR (SenseVoice/Qwen3-ASR/Whisper),通过 sherpa-onnx。
/// 从 aura-asr 迁入——audio-aura 不再直接依赖 sherpa-onnx。
#[cfg(feature = "speech")]
pub mod onnx;

pub use config::ProviderKind;

// ── VAD 数据契约(纯数据,不依赖 sherpa;由 onnx 模块与 aura-asr 的 EnergyVad 共用)──

/// VAD 事件类型(port of livekit VadEventKind)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEventKind {
    StartOfSpeech,
    EndOfSpeech,
}

/// 一次 VAD 事件(port of livekit VADEvent + Silero state machine)。
#[derive(Debug, Clone)]
pub struct VadEvent {
    pub kind: VadEventKind,
    /// The accumulated utterance PCM (only on EndOfSpeech; empty on StartOfSpeech).
    pub pcm: Vec<i16>,
}

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
