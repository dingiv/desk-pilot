//! Provider 配置类型 (下游塞进 aura.json 等配置文件)。

use serde::{Deserialize, Serialize};

/// 一个 task 的后端选择: local (lib 嵌入) 或 remote (HTTP 服务)。
///
/// 序列化为 internally-tagged: `{"kind":"local"}` 或 `{"kind":"remote","endpoint":"..."}`，
/// 方便和 task-specific 字段 (backend/provider/threads/model) 在下游配置里平铺。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ProviderKind {
    /// 本地 lib 嵌入 (sherpa / mistral.rs / candle)，进程内推理。
    #[default]
    Local,
    /// 远程 HTTP 服务 (vLLM / SGLang / qwen3-asr-rs server / 云端)，OpenAI 兼容协议。
    Remote { endpoint: String },
}

impl ProviderKind {
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::Remote { endpoint } => Some(endpoint),
            _ => None,
        }
    }
}
