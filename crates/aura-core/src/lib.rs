//! audio-aura-core — Stage1→Stage2 pipeline + Stage2 calibrator + context window +
//! prompt builder + storage (audio archive + turn log + recent ring). Merged from
//! the former aura-core (composer), aura-dcl (calibrator/prompt/context), and
//! aura-store (hub/archive/wav).
//!
//! External dep graph: this crate → audio-aura-asr; daemon/native → this crate.
//!
//! `composer` (the [`Pipeline`]) composes the ONNX-side `Stage1Executor` → Stage2, so it is
//! gated behind the `asr` feature (= `audio-aura-asr/onnx`) — the default build stays light
//! (calibrator/context/storage only, no sherpa-onnx). Enable it with `features = ["asr"]`.

pub mod archive;
pub mod calibrator;
#[cfg(feature = "asr")]
pub mod composer;
pub mod hub;
pub mod prompt;
pub mod wav;

pub use calibrator::{Stage2Calibrator, Stage2CalibratorImpl};
#[cfg(feature = "asr")]
pub use composer::{Pipeline, TurnEvent};
pub use prompt::PromptBuilder;
pub use hub::{FinalTurn, Storage, TurnRecord};

// ── Calibrator (mistral.rs Qwen3-1.7B GGUF loader) — from the former aura-dcl ──

use std::sync::Arc;

use anyhow::Result;
use mistralrs::{GgufModelBuilder, Model, TextMessageRole, TextMessages};
use tokio::runtime::Runtime;

/// Resident engine: GGUF model loaded once, kept warm. Holds its own tokio runtime so
/// callers (napi Task threadpool, or the daemon via spawn_blocking) can call synchronously.
pub struct Calibrator {
    model: Arc<Model>,
    rt: Arc<Runtime>,
}

impl Calibrator {
    pub fn load(model_dir: &str, model_file: &str) -> Result<Self> {
        let rt = Runtime::new()?;
        let model = rt.block_on(async {
            GgufModelBuilder::new(model_dir.to_string(), vec![model_file.to_string()])
                .build()
                .await
        })?;
        Ok(Self { model: Arc::new(model), rt: Arc::new(rt) })
    }

    /// Load by model file name only — the model **directory** is resolved via shared namespace
    /// `MODELS` (declared in this crate's `Cargo.toml`).
    pub fn load_default(model_file: &str) -> Result<Self> {
        let fs = shared::loader!();
        let dir = fs
            .resolve("MODELS::")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::load(&dir, model_file)
    }

    /// Run the merged 整流+路由 on one utterance; returns the model's raw JSON text.
    pub fn calibrate_blocking(
        &self,
        raw_text: &str,
        context: Option<&str>,
        hotwords: &[String],
    ) -> Result<String> {
        let mut pb = crate::prompt::PromptBuilder::new(raw_text).hotwords(hotwords);
        if let Some(c) = context {
            pb = pb.context(c);
        }
        let (system, user) = pb.build();
        self.infer(&system, &user)
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

impl dp_models::LlmProvider for Calibrator {
    fn complete(&self, system: &str, user: &str) -> Result<String> {
        self.infer(system, user)
    }
}
