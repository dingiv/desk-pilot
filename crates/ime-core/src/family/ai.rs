//! AIFamily — context-aware sentence prediction (stub).
//!
//! Reserved for future LLM-based prediction. When the user has built up
//! context (e.g. "我来自遥远的东方大"), this family can generate full
//! sentence continuations like "陆，那里是太阳升起的地方。"
//!
//! Architecture: this family will call an LLM endpoint (local or remote)
//! with the context + current input, and the model returns ranked completions.
//! The default stub generates nothing until an LLM backend is configured.

use super::{CandidateFamily, InputContext, ScoredCandidate};

pub struct AiFamily {
    enabled: bool,
}

impl AiFamily {
    pub fn new() -> Self {
        AiFamily { enabled: true }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl Default for AiFamily {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateFamily for AiFamily {
    fn name(&self) -> &'static str { "ai" }
    fn priority(&self) -> u32 { 40 }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn top_n(&self) -> usize {
        2 // AI predictions are expensive — only send the top 2
    }

    fn predict(&self, _input: &str) -> Vec<ScoredCandidate> {
        // No AI backend configured — return nothing.
        Vec::new()
    }

    fn predict_with_context(&self, input: &str, ctx: &InputContext) -> Vec<ScoredCandidate> {
        if ctx.recent_text.is_empty() {
            return Vec::new();
        }
        // TODO: when LLM backend is available:
        //   1. Build prompt from context.recent_text + input
        //   2. Call LLM for completions
        //   3. Return top-k completions as ScoredCandidates
        //
        // Example:
        //   ctx = "我来自遥远的东方大"
        //   input = "lu"
        //   → LLM returns ["陆，那里是太阳升起的地方。", ...]
        let _ = input;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_family_stub_returns_empty() {
        let fam = AiFamily::new();
        assert!(fam.predict("lu").is_empty());
    }

    #[test]
    fn ai_family_with_context_stub() {
        let fam = AiFamily::new();
        let mut ctx = InputContext::new();
        ctx.update("我来自遥远的东方大");
        // Stub returns empty until LLM backend is configured.
        assert!(fam.predict_with_context("lu", &ctx).is_empty());
    }
}
