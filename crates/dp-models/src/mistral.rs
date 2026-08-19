//! mistral — 本地 LLM (mistral.rs Qwen GGUF 加载器)。
//!
//! 从 aura-core 迁入 (2026-08-18): 把 mistralrs 重依赖隔离到 dp-models, 让业务层只保留流程
//! 逻辑。dp-models 是通用模型提供库, 本地 LLM 是其中一种实现 (供 aura Stage2 或任何上层用)。
//!
//! [`MistralLlm`] 实现 [`crate::LlmProvider`] + [`crate::ModelProvider`]; 选 local/remote
//! 的工厂在上层 (如 aura-core 的 `stage2_calibrator`)。

use std::sync::Arc;

use anyhow::Result;
use mistralrs::{GgufModelBuilder, Model, TextMessageRole, TextMessages};
use tokio::runtime::Runtime;

use crate::{LlmProvider, ModelProvider};

/// 本地 LLM (mistral.rs GGUF): 模型加载一次、常驻。持有自己的 tokio runtime, 调用方可同步
/// 使用 (napi Task threadpool / daemon worker 线程)。
pub struct MistralLlm {
    model: Arc<Model>,
    rt: Arc<Runtime>,
}

impl MistralLlm {
    /// Load a GGUF model from `model_dir` + `model_file`.
    pub fn load(model_dir: &str, model_file: &str) -> Result<Self> {
        let rt = Runtime::new()?;
        let model = rt.block_on(async {
            GgufModelBuilder::new(model_dir.to_string(), vec![model_file.to_string()])
                .build()
                .await
        })?;
        Ok(Self { model: Arc::new(model), rt: Arc::new(rt) })
    }

    /// Load by model file name only — the model **directory** is resolved via the shared
    /// `MODELS` namespace (declared in this crate's `Cargo.toml`; dev = workspace
    /// `assets/models`, prod = `~/.desk-pilot/models`).
    pub fn load_default(model_file: &str) -> Result<Self> {
        let fs = shared::loader!();
        let dir = fs
            .resolve("MODELS::")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::load(&dir, model_file)
    }

    /// Raw one-shot chat: send a (system, user) pair.
    pub fn infer(&self, system: &str, user: &str) -> Result<String> {
        let messages = TextMessages::new()
            .add_message(TextMessageRole::System, system)
            .add_message(TextMessageRole::User, user);
        let resp = self.rt.block_on(self.model.send_chat_request(messages))?;
        Ok(resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default())
    }
}

impl ModelProvider for MistralLlm {
    fn kind(&self) -> &'static str {
        "local-mistral"
    }
}

impl LlmProvider for MistralLlm {
    fn complete(&self, system: &str, user: &str) -> Result<String> {
        self.infer(system, user)
    }
}
