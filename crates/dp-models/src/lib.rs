//! dp-models — desk-pilot 的本地模型 Provider 抽象层 + 内部 SDK。
//!
//! 定义统一的 task trait (`AsrProvider`/`LlmProvider`/`VlmProvider`)，下游 (aura / visual-rover)
//! 通过 trait 对象调用，不关心推理是 **local** (lib 嵌入: sherpa-onnx 语音栈) 还是
//! **remote** (OpenAI 兼容 HTTP: dp-router / vLLM / 任意 OpenAI 兼容服务)。
//!
//! 本 crate 只做抽象 + remote 实现；local 实现留在各专业 crate (OnnxAsr)，它们
//! `impl dp_models::XxxProvider`。工厂 (选 local/remote) 在各 app
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

/// **模型提供者 (伞形 marker)** — 每个 provider 实现类标识自己的 `kind`。真正的能力是
/// 各领域的 trait object ([`AsrProvider`] / [`LlmProvider`] / [`VlmProvider`]), 一个
/// provider 实现类额外 impl 它支持的那些能力 trait。上层使用者按需实例化具体实现
/// (如 `HttpAsr::new(endpoint)` / `OnnxAsr::load(cfg)` / `Calibrator::load(...)`), 再按
/// 能力取用。dp-models 是通用模型提供库, 不只给 aura 用。
pub trait ModelProvider: Send + Sync {
    /// 实现家族标签, 如 `"local-onnx"` / `"remote-http"` / `"local-mistral"`。
    fn kind(&self) -> &'static str;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpAsr, HttpLlm, HttpVlm};

    /// 伞形 marker 的契约: 每个实现类标识自己的 kind, 上层按 kind/能力取用。
    #[test]
    fn providers_identify_their_kind() {
        assert_eq!(HttpAsr::new("http://x", "m").kind(), "remote-http");
        assert_eq!(HttpLlm::new("http://x", "m").kind(), "remote-http");
        assert_eq!(HttpVlm::new("http://x", "m").kind(), "remote-http");
    }
}
