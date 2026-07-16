//! Stage2Calibrator — wraps the [`RouterEngine`] + a rolling [`ContextWindow`] + an LLM-layer
//! hotword store, and turns a Stage1 [`Utterance`] into a [`Decision`] (calibrated text + intent
//! + reply + task). It owns prompt construction (hotwords + (raw,calibrated) pairs context +
//! few-shot), so the router is left as a pure inference primitive (`RouterEngine::infer`).
//!
//! The hotword store is `Arc<Mutex<Vec<String>>>` and is **shared with Stage3** — when the agent
//! (or desktop-pet) adds a correction hotword, the very next `calibrate` picks it up. That is the
//! Stage3 → Stage2 feedback channel.

use std::sync::{Arc, Mutex};

use audio_aura_asr::Utterance;

use crate::context::ContextWindow;
use crate::prompt::PromptBuilder;
use crate::{parse_decision, Decision, RouterEngine};

/// Turns a finalized utterance into a calibrated Decision. Implementations own their context.
pub trait Stage2Calibrator {
    fn calibrate(&mut self, utterance: &Utterance) -> Decision;
}

/// Default Stage2 calibrator over the local Qwen router. Holds a rolling (raw,calibrated) pairs
/// window and reads the latest hotwords (shared with Stage3) on every call.
pub struct RouterStage2Calibrator {
    router: RouterEngine,
    ctx_win: ContextWindow,
    /// Shared with Stage3 — the feedback channel. Read fresh on every calibrate.
    hotwords: Arc<Mutex<Vec<String>>>,
    few_shot: Vec<(String, String)>,
}

impl RouterStage2Calibrator {
    /// `hotwords` is shared (clone the Arc from wherever Stage3 holds it); `router` is moved in.
    pub fn new(router: RouterEngine, hotwords: Arc<Mutex<Vec<String>>>) -> Self {
        Self { router, ctx_win: ContextWindow::new(5), hotwords, few_shot: Vec::new() }
    }

    /// Rolling context capacity (number of (raw,calibrated) pairs kept). Default 4.
    pub fn with_context_capacity(mut self, cap: usize) -> Self {
        self.ctx_win = ContextWindow::new(cap);
        self
    }

    /// Override the default few-shot examples. Pass empty to disable few-shot.
    pub fn with_few_shot(mut self, examples: Vec<(String, String)>) -> Self {
        self.few_shot = examples;
        self
    }
}

impl Stage2Calibrator for RouterStage2Calibrator {
    fn calibrate(&mut self, utterance: &Utterance) -> Decision {
        let route = utterance.route_text();
        let ctx = if self.ctx_win.is_empty() {
            None
        } else {
            Some(self.ctx_win.as_pairs())
        };
        // Read the latest hotwords (may have grown since last call via Stage3 feedback).
        let hotwords = self.hotwords.lock().unwrap().clone();

        let mut pb = PromptBuilder::new(route).hotwords(&hotwords);
        if let Some(c) = ctx.as_deref() {
            pb = pb.context(c);
        }
        if !self.few_shot.is_empty() {
            pb = pb.few_shot(&self.few_shot);
        }
        let (system, user) = pb.build();

        let raw = self.router.infer(&system, &user).unwrap_or_default();
        let decision = parse_decision(&raw, route);

        // Roll the context window: this utterance's (raw→calibrated) becomes a pattern the LLM
        // can learn from on the next turn.
        self.ctx_win.push(route, &decision.calibrated_text, &decision.intent);
        decision
    }
}

#[cfg(test)]
mod tests {
    use audio_aura_asr::Utterance;

    fn utterance(raw: &str) -> Utterance {
        Utterance {
            seq: 1,
            raw_text: raw.into(),
            streaming_text: String::new(),
            duration_ms: 1000.0,
            at_s: 1.0,
        }
    }

    #[test]
    fn route_text_falls_back_to_streaming() {
        // Stage2 calibrates on the batch final; when it's empty, the streaming final is used.
        let mut u = utterance("real text");
        u.raw_text = "   ".into();
        u.streaming_text = "stream fallback".into();
        assert_eq!(u.route_text(), "stream fallback");
    }

    #[test]
    fn shared_hotword_store_visible_to_both() {
        // The Stage3→Stage2 feedback channel: the same Arc<Mutex<Vec<String>>> is mutated by
        // Stage3 and read by Stage2. (Calibrator construction needs the real model, exercised in
        // the example; here we just prove the sharing primitive.)
        use std::sync::{Arc, Mutex};
        let store: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec!["Rust".into()]));
        let reader = Arc::clone(&store);
        store.lock().unwrap().push("Bevy".into()); // Stage3 adds
        assert_eq!(*reader.lock().unwrap(), vec!["Rust".to_string(), "Bevy".to_string()]);
    }

    #[test]
    fn context_window_rolls_pairs() {
        use crate::context::ContextWindow;
        let mut w = ContextWindow::new(2);
        w.push("rost语言", "Rust语言", "task");
        w.push("B位引擎", "Bevy引擎", "task");
        let pairs = w.as_pairs();
        assert!(pairs.contains("rost语言") && pairs.contains("Rust语言"));
    }
}
