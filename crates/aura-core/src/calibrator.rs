//! Stage2Calibrator — wraps the [`Calibrator`] + a rolling [`ContextWindow`] + an LLM-layer
//! hotword store, and turns a Stage1 [`Utterance`] into a [`Decision`] (calibrated text + intent
//! + reply + task). It owns prompt construction (hotwords + (raw,calibrated) pairs context +
//! few-shot), so the calibrator is left as a pure inference primitive (`Calibrator::infer`).
//!
//! The hotword store is `Arc<Mutex<Vec<String>>>` and is **shared with Stage3** — when the agent
//! (or desktop-pet) adds a correction hotword, the very next `calibrate` picks it up. That is the
//! Stage3 → Stage2 feedback channel.

use std::sync::{Arc, Mutex};

use audio_aura_asr::Utterance;

use crate::context::ContextWindow;
use crate::prompt::PromptBuilder;
use crate::Decision;

/// Turns a finalized utterance into a calibrated Decision. Implementations own their context.
///
/// Stage2's correction pass (纠偏) is driven by the two [`audio_aura_asr::Stage1Action`]s
/// Stage1 emits — both land here: the `Batch` action (普通 batch, pause ≈ 1s) via
/// [`calibrate_provisional`](Self::calibrate_provisional), the `MergeBatch` action (大合并,
/// pause ≈ 5s, settled paragraph) via [`calibrate`](Self::calibrate).
pub trait Stage2Calibrator: Send {
    /// Authoritative calibration of a SETTLED utterance (a `MergeBatch` action) — commits the
    /// (raw→calibrated) pair to the ContextWindow so subsequent utterances can reference it.
    fn calibrate(&mut self, utterance: &Utterance) -> Decision;
    /// Provisional calibration of an IN-PROGRESS utterance (a `Batch` action from an absorbed
    /// fragment) — same calibration logic, but does NOT commit to the ContextWindow. A growing
    /// utterance's intermediate states must not pollute the window; only its settle `MergeBatch`
    /// enters context. Default delegates to [`calibrate`](Self::calibrate) for impls that don't
    /// track context.
    fn calibrate_provisional(&mut self, utterance: &Utterance) -> Decision {
        self.calibrate(utterance)
    }
}

/// Default Stage2 calibrator over the local Qwen calibrator. Holds a rolling (raw,calibrated) pairs
/// window and reads the latest hotwords (shared with Stage3) on every call.
pub struct Stage2CalibratorImpl {
    llm: Arc<dyn dp_models::LlmProvider>,
    ctx_win: ContextWindow,
    /// Shared with Stage3 — the feedback channel. Read fresh on every calibrate.
    hotwords: Arc<Mutex<Vec<String>>>,
    /// User corrections (raw→corrected pairs), shared with daemon's POST /api/correct handler.
    /// Read fresh on every calibrate — the correction feedback channel.
    corrections: Arc<Mutex<Vec<(String, String)>>>,
    few_shot: Vec<(String, String)>,
}

impl Stage2CalibratorImpl {
    /// `hotwords` is shared (clone the Arc from wherever Stage3 holds it); `llm` is the local
    /// `Calibrator` or a remote `HttpLlm` (as `Arc<dyn LlmProvider>`).
    pub fn new(
        llm: Arc<dyn dp_models::LlmProvider>,
        hotwords: Arc<Mutex<Vec<String>>>,
        corrections: Arc<Mutex<Vec<(String, String)>>>,
    ) -> Self {
        Self { llm, ctx_win: ContextWindow::new(5), hotwords, corrections, few_shot: Vec::new() }
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

impl Stage2Calibrator for Stage2CalibratorImpl {
    fn calibrate(&mut self, utterance: &Utterance) -> Decision {
        self.calibrate_inner(utterance, true)
    }
    fn calibrate_provisional(&mut self, utterance: &Utterance) -> Decision {
        self.calibrate_inner(utterance, false)
    }
}

impl Stage2CalibratorImpl {
    /// Shared calibration core. `commit` controls whether the (raw→calibrated) pair enters the
    /// ContextWindow — only the settled `Final` commits; provisionals (`Revise`) do not, so a
    /// growing utterance's intermediate states don't pollute the window.
    fn calibrate_inner(&mut self, utterance: &Utterance, commit: bool) -> Decision {
        let route = utterance.route_text();
        // ContextWindow disabled — Qwen2.5-3B copies previous calibrated text instead of
        // using it as reference. Re-enable only with a model >= 7B that follows "不要复读" reliably.
        // let ctx = if self.ctx_win.is_empty() { None } else { Some(self.ctx_win.as_pairs()) };
        let ctx: Option<String> = None;
        // Read the latest hotwords (may have grown since last call via Stage3 feedback).
        let hotwords = self.hotwords.lock().unwrap().clone();

        let mut pb = PromptBuilder::new(route).hotwords(&hotwords);
        // Streaming ref disabled — Qwen3-1.7B can't reconcile dual transcripts reliably;
        // batch Qwen3-ASR is already high quality, Stage2 just cleans up minor errors.
        // pb = pb.streaming_ref(&utterance.streaming_text);
        if let Some(c) = ctx.as_deref() {
            pb = pb.context(c);
        }
        if !self.few_shot.is_empty() {
            pb = pb.few_shot(&self.few_shot);
        }
        // User corrections (raw→corrected) — authoritative few-shot, highest priority.
        let corrections = self.corrections.lock().unwrap().clone();
        if !corrections.is_empty() {
            pb = pb.corrections(&corrections);
        }
        let (system, user) = pb.build();
        tracing::debug!(target: "stage2::prompt", commit, system = %system, user = %user, "calibrate prompt");

        // LLM returns plain text — no JSON parsing needed.
        let calibrated = self.llm.complete(&system, &user).unwrap_or_else(|_| route.to_string());
        let decision = Decision {
            calibrated_text: calibrated.trim().to_string(),
            intent: "chat".into(),
            reply: String::new(),
            task: None,
        };

        // Roll the context window ONLY for committed (settled) calibrations.
        if commit {
            self.ctx_win.push(route, &decision.calibrated_text, &decision.intent);
        }
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
            pcm: Vec::new(),
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
