//! EmojiFamily — emoji character prediction (stub).
//!
//! Reserved for future implementation. Will match shorthand patterns
//! like ":smile" → 😊, ":heart" → ❤️, etc.

use super::{CandidateFamily, InputContext, ScoredCandidate};

pub struct EmojiFamily {
    enabled: bool,
}

impl EmojiFamily {
    pub fn new() -> Self {
        EmojiFamily { enabled: true }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl Default for EmojiFamily {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateFamily for EmojiFamily {
    fn name(&self) -> &'static str { "emoji" }
    fn priority(&self) -> u32 { 50 }
    fn enabled(&self) -> bool { self.enabled }
    fn top_n(&self) -> usize { 4 }

    fn predict(&self, _input: &str) -> Vec<ScoredCandidate> {
        Vec::new() // stub — no emoji patterns yet
    }

    fn predict_with_context(&self, input: &str, _ctx: &InputContext) -> Vec<ScoredCandidate> {
        self.predict(input)
    }
}
