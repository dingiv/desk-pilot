//! mistral — 本地 Stage2 LLM (mistral.rs Qwen GGUF 加载器)。
//!
//! 从 aura-core 迁入 (2026-08-18): 把 mistralrs 重依赖隔离到 dp-models, 让 aura-core 只保留
//! Stage1/Stage2 流程逻辑。今后 batch ASR / Stage2 / Stage3 都走远程, 本地 LLM 是过渡形态。
//!
//! [`Calibrator`] 实现 [`crate::LlmProvider`], 供 Stage2 联合整流用; 选 local/remote 的
//! 工厂在 aura-core 的 `stage2_calibrator`。

use std::sync::Arc;

use anyhow::Result;
use mistralrs::{GgufModelBuilder, Model, TextMessageRole, TextMessages};
use tokio::runtime::Runtime;

use crate::LlmProvider;

/// Resident engine: GGUF model loaded once, kept warm. Holds its own tokio runtime so
/// callers (napi Task threadpool, or the daemon via spawn_blocking) can call synchronously.
pub struct Calibrator {
    model: Arc<Model>,
    rt: Arc<Runtime>,
}

impl Calibrator {
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

impl LlmProvider for Calibrator {
    fn complete(&self, system: &str, user: &str) -> Result<String> {
        self.infer(system, user)
    }
}
