//! audio-aura-router — the local router SLM as pure Rust logic (no napi), shared by the napi shim
//! (`native/`) and the standalone daemon (`audio-aura-core`). Runs Qwen3-1.7B (GGUF) via mistral.rs and
//! does the merged 口语整流 + 意图路由 in one call, returning the model's raw JSON text. `parse_decision`
//! normalizes that into a `Decision` (mirrors the TS `local-router.ts` tolerance for a flattened task).

use std::sync::Arc;

use anyhow::Result;
use mistralrs::{GgufModelBuilder, Model, TextMessageRole, TextMessages};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

pub mod context;
pub mod prompt;
pub mod calibrator;


/// Resident engine: GGUF model loaded once, kept warm. Holds its own tokio runtime so callers
/// (napi Task threadpool, or the daemon via spawn_blocking) can call `route_blocking` synchronously.
pub struct RouterEngine {
    model: Arc<Model>,
    rt: Arc<Runtime>,
}

impl RouterEngine {
    /// Load the GGUF model (blocks ~seconds, once). GPU is used when built with `--features cuda`.
    pub fn load(model_dir: &str, model_file: &str) -> Result<Self> {
        let rt = Runtime::new()?;
        let model = rt.block_on(async {
            GgufModelBuilder::new(model_dir.to_string(), vec![model_file.to_string()])
                .build()
                .await
        })?;
        Ok(Self {
            model: Arc::new(model),
            rt: Arc::new(rt),
        })
    }

    /// Load by model file name only — the model **directory** is resolved via aura-fs namespace
    /// `MODELS` (declared in this crate's `Cargo.toml`). Dev: `<workspace>/native/models/`;
    /// prod: `~/.audio-aura/models/`. The caller never sees the directory path.
    pub fn load_default(model_file: &str) -> Result<Self> {
        let fs = aura_fs::loader!();
        let dir = fs
            .resolve("MODELS::")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::load(&dir, model_file)
    }

    /// Run the merged 整流+路由 on one utterance; returns the model's raw JSON text.
    /// `context` = recent-dialogue text prepended for homophone/intent context.
    /// `hotwords` = user-specific terms whose homophones should be corrected to these spellings.
    pub fn route_blocking(
        &self,
        raw_text: &str,
        context: Option<&str>,
        hotwords: &[String],
    ) -> Result<String> {
        let mut pb = prompt::PromptBuilder::new(raw_text).hotwords(hotwords);
        if let Some(c) = context {
            pb = pb.context(c);
        }
        let (system, user) = pb.build();
        self.infer(&system, &user)
    }

    /// Raw one-shot chat: send a (system, user) pair, return the model's text response. Use this
    /// (with a hand-built [`prompt::PromptBuilder`]) when you need full prompt control — e.g. the
    /// Stage2 calibrator injects custom few-shot examples + reads a shared hotword store.
    pub fn infer(&self, system: &str, user: &str) -> Result<String> {
        let messages = TextMessages::new()
            .add_message(TextMessageRole::System, system)
            .add_message(TextMessageRole::User, user);
        let resp = self.rt.block_on(self.model.send_chat_request(messages))?;
        let text = resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        Ok(text)
    }
}

// ── decision parsing (mirrors TS local-router.ts normalization) ─────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub capability: String,
    pub brief: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub calibrated_text: String,
    pub intent: String, // "chat" | "task"
    pub reply: String,
    pub task: Option<TaskSpec>,
}

/// Extract a JSON object from possibly-fenced model text: first '{' to last '}'.
fn extract_json(text: &str) -> Option<serde_json::Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

/// Normalize raw model text into a Decision. Tolerates the small model flattening `task` to a string.
pub fn parse_decision(raw: &str, fallback_text: &str) -> Decision {
    let v = extract_json(raw);
    let obj = v.as_ref().and_then(|v| v.as_object());

    let calibrated_text = obj
        .and_then(|o| o.get("calibrated_text"))
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_text)
        .to_string();

    let intent = match obj.and_then(|o| o.get("intent")).and_then(|s| s.as_str()) {
        Some("task") => "task",
        _ => "chat",
    }
    .to_string();

    let reply = obj
        .and_then(|o| o.get("reply"))
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        // Sensible intent-aware fallback when the small model leaves reply empty — better than a
        // single generic line for every utterance.
        .unwrap_or_else(|| {
            if intent == "task" {
                "好的，我来写。".to_string()
            } else {
                "嗯，好的。".to_string()
            }
        });

    let mut task = None;
    if intent == "task" {
        let t = obj.and_then(|o| o.get("task"));
        let brief_sibling = obj
            .and_then(|o| o.get("brief"))
            .and_then(|s| s.as_str())
            .unwrap_or(&calibrated_text);
        task = match t {
            Some(serde_json::Value::Object(m)) => {
                let capability = m
                    .get("capability")
                    .and_then(|s| s.as_str())
                    .unwrap_or("write")
                    .to_string();
                let brief = m
                    .get("brief")
                    .and_then(|s| s.as_str())
                    .unwrap_or(brief_sibling)
                    .to_string();
                Some(TaskSpec { capability, brief })
            }
            Some(serde_json::Value::String(s)) => Some(TaskSpec {
                capability: s.clone(),
                brief: brief_sibling.to_string(),
            }),
            _ => Some(TaskSpec {
                capability: "write".to_string(),
                brief: brief_sibling.to_string(),
            }),
        };
    }

    Decision {
        calibrated_text,
        intent,
        reply,
        task,
    }
}
