//! Decision types + LLM output parsing (extract JSON from markdown fences → Decision).

use serde::{Deserialize, Serialize};

/// The result of Stage2 calibration: calibrated text + intent + reply + optional task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub calibrated_text: String,
    pub intent: String,           // "chat" | "task"
    pub reply: String,
    pub task: Option<TaskSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub capability: String,
    pub brief: String,
}

/// Parse the LLM's raw text (possibly inside markdown fences) into a [`Decision`].
/// `fallback_raw` is used when parsing fails or `calibrated_text` is empty.
pub fn parse_decision(raw_json: &str, fallback_raw: &str) -> Decision {
    let text = extract_json(raw_json);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    Decision {
        calibrated_text: v
            .get("calibrated_text")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_raw)
            .to_string(),
        intent: v
            .get("intent")
            .and_then(|v| v.as_str())
            .unwrap_or("chat")
            .to_string(),
        reply: v
            .get("reply")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        task: match v.get("task") {
            Some(t) if !t.is_null() => Some(TaskSpec {
                capability: t
                    .get("capability")
                    .and_then(|v| v.as_str())
                    .unwrap_or("write")
                    .to_string(),
                brief: t
                    .get("brief")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
            _ => None,
        },
    }
}

/// Extract the first JSON object from text, stripping markdown fences (` ```json ` blocks).
fn extract_json(raw: &str) -> String {
    let s = raw.trim();
    // strip ```json ... ``` fences
    if let Some(inner) = s
        .strip_prefix("```json")
        .and_then(|t| t.strip_suffix("```"))
        .or_else(|| s.strip_prefix("```").and_then(|t| t.strip_suffix("```")))
    {
        return inner.trim().to_string();
    }
    // find first '{' … last '}'
    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        return s[start..=end].to_string();
    }
    s.to_string()
}
