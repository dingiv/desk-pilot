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
//! 所有 trait **同步**(兼容存量消费方);远程 Http* 家族另提供**原生异步轨**
//! (`*_async` inherent 方法)+ [`AsyncAsr`]/[`AsyncLlm`] 路由(构造期定型,
//! 统一异步入口:Http 原生 await,本地同步 provider 走 spawn_blocking 桥)。

use anyhow::anyhow;

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
///
/// 同步 trait(兼容存量消费方)。远程实现 [`crate::HttpAsr`] 额外提供原生异步轨
/// `recognize_async` —— tokio 上下文优先用它(可 await、超时可真取消)。
pub trait AsrProvider: Send + Sync {
    fn recognize(&self, pcm: &[i16], sample_rate: u32) -> anyhow::Result<String>;
}

/// 文本 LLM (如 Stage2 整流/路由): (system, user) -> 文本。
///
/// 同步 trait(兼容存量消费方)。远程实现 [`crate::HttpLlm`] 额外提供原生异步轨
/// `complete_async` —— tokio 上下文优先用它。
pub trait LlmProvider: Send + Sync {
    fn complete(&self, system: &str, user: &str) -> anyhow::Result<String>;
}

/// 视觉语言模型 (VLM): (system, user, image_png) -> 文本。local 实现留 visual-rover 未来。
///
/// 同步 trait(兼容存量消费方)。远程实现 [`crate::HttpVlm`] 额外提供原生异步轨
/// `complete_async` —— tokio 上下文优先用它。
pub trait VlmProvider: Send + Sync {
    fn complete(&self, system: &str, user: &str, image_png: &[u8]) -> anyhow::Result<String>;
}

// ── 异步统一入口(路由,构造期定型)────────────────────────────────────────────
//
// 把"任意 provider"变成一个可 await 的调用面:**远程 Http 原生异步**(超时可被
// `tokio::time::timeout` 真取消);**本地/任意同步 provider 走 spawn_blocking 桥**
// (Arc 进闭包 = 'static;CPU 密集推理本就该在 blocking pool)。上层(aura-core 的
// batch 任务)面对路由类型,不必知道部署形态。

/// [`AsrProvider`] 的异步统一入口(ASR 路由)。
#[derive(Clone)]
pub enum AsyncAsr {
    /// 远程 HTTP —— 原生 `reqwest` 异步([`HttpAsr::recognize_async`])。
    Http(std::sync::Arc<http::HttpAsr>),
    /// 本地 ONNX 等同步 provider —— `spawn_blocking` 桥。
    Blocking(std::sync::Arc<dyn AsrProvider>),
}

impl AsyncAsr {
    /// 异步轨:Http 原生 await;Blocking = spawn_blocking(Arc clone 进闭包)。
    pub async fn recognize(&self, pcm: &[i16], sample_rate: u32) -> anyhow::Result<String> {
        match self {
            AsyncAsr::Http(h) => h.recognize_async(pcm, sample_rate).await,
            AsyncAsr::Blocking(p) => {
                let pcm = pcm.to_vec();
                let p = std::sync::Arc::clone(p);
                tokio::task::spawn_blocking(move || p.recognize(&pcm, sample_rate))
                    .await
                    .map_err(|e| anyhow!("ASR blocking join error: {e}"))?
            }
        }
    }

    /// 同步轨(同步消费方;Http → 原生同步轨,Blocking → 直调)。
    pub fn recognize_sync(&self, pcm: &[i16], sample_rate: u32) -> anyhow::Result<String> {
        match self {
            AsyncAsr::Http(h) => h.recognize(pcm, sample_rate),
            AsyncAsr::Blocking(p) => p.recognize(pcm, sample_rate),
        }
    }
}

/// [`LlmProvider`] 的异步统一入口(LLM 路由)。
#[derive(Clone)]
pub enum AsyncLlm {
    /// 远程 HTTP —— 原生 `reqwest` 异步([`HttpLlm::complete_async`])。
    Http(std::sync::Arc<http::HttpLlm>),
    /// 本地 LLM 等同步 provider —— `spawn_blocking` 桥。
    Blocking(std::sync::Arc<dyn LlmProvider>),
}

impl AsyncLlm {
    /// 异步轨:Http 原生 await;Blocking = spawn_blocking(Arc clone 进闭包)。
    pub async fn complete(&self, system: &str, user: &str) -> anyhow::Result<String> {
        match self {
            AsyncLlm::Http(h) => h.complete_async(system, user).await,
            AsyncLlm::Blocking(p) => {
                let (s, u) = (system.to_string(), user.to_string());
                let p = std::sync::Arc::clone(p);
                tokio::task::spawn_blocking(move || p.complete(&s, &u))
                    .await
                    .map_err(|e| anyhow!("LLM blocking join error: {e}"))?
            }
        }
    }

    /// 同步轨(同步消费方;Http → 原生同步轨,Blocking → 直调)。
    pub fn complete_sync(&self, system: &str, user: &str) -> anyhow::Result<String> {
        match self {
            AsyncLlm::Http(h) => h.complete(system, user),
            AsyncLlm::Blocking(p) => p.complete(system, user),
        }
    }
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
