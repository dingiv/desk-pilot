//! Stage2Calibrator — 联合整流 (joint calibration): turns a window's [`VadSegment`] texts into
//! polished text (加标点 / 修同音错字 / 英文规范 / 专有名词修正). Wraps an
//! [`dp_models::LlmProvider`] (local mistral.rs or remote) + the LLM-layer hotword store
//! (shared with Stage3) + the user-correction ring (shared with POST /api/correct).
//!
//! STATELESS by design (2026-08-17 边界范式): the two trigger events carry the window
//! themselves — `Stage1Event::Batch { segments }` brings ALL segments of the current window
//! so far, `Stage1Event::WindowEdge { window }` brings the settled snapshot. No internal
//! left-boundary bookkeeping exists to desync; "移动左边界" is simply the next event's
//! payload being the new window. The old ContextWindow (disabled — 3B 复读) is deleted:
//! cross-sentence context now enters through the joint input itself.

use std::sync::{Arc, Mutex};

use audio_aura_asr::{VadSegment, VadWindow, WindowId};

use crate::prompt::PromptBuilder;

/// Stage2's correction pass (纠偏/整流), driven by the two Stage1 events:
/// - `Stage1Event::Batch` → [`calibrate_window`](Self::calibrate_window) — provisional joint
///   calibration of every segment in the current window (multi-sentence);
/// - `Stage1Event::WindowEdge` → [`calibrate_final`](Self::calibrate_final) — the settled
///   window's authoritative calibration.
pub trait Stage2Calibrator: Send {
    /// Provisional joint calibration (per Batch). Input = ALL segments so far.
    fn calibrate_window(&mut self, window_id: WindowId, segments: &[VadSegment]) -> String;
    /// Authoritative calibration (per WindowEdge). Runs on the window-level batch text
    /// (falling back to the concat of per-segment best texts when the re-run failed).
    fn calibrate_final(&mut self, window: &VadWindow) -> String;
}

/// Default Stage2 calibrator over an [`dp_models::LlmProvider`]. Reads the latest hotwords
/// (shared with Stage3) and user corrections on every call.
pub struct Stage2CalibratorImpl {
    llm: Arc<dyn dp_models::LlmProvider>,
    /// Shared with Stage3 — the feedback channel. Read fresh on every calibrate.
    hotwords: Arc<Mutex<Vec<String>>>,
    /// User corrections (raw→corrected pairs), shared with daemon's POST /api/correct handler.
    /// Read fresh on every calibrate — the correction feedback channel.
    corrections: Arc<Mutex<Vec<(String, String)>>>,
}

impl Stage2CalibratorImpl {
    /// `hotwords` is shared (clone the Arc from wherever Stage3 holds it); `llm` is the local
    /// `Calibrator` or a remote `HttpLlm` (as `Arc<dyn LlmProvider>`).
    pub fn new(
        llm: Arc<dyn dp_models::LlmProvider>,
        hotwords: Arc<Mutex<Vec<String>>>,
        corrections: Arc<Mutex<Vec<(String, String)>>>,
    ) -> Self {
        Self { llm, hotwords, corrections }
    }
}

impl Stage2Calibrator for Stage2CalibratorImpl {
    fn calibrate_window(&mut self, _window_id: WindowId, segments: &[VadSegment]) -> String {
        // One line per segment — the joint input IS the cross-sentence context.
        let texts: Vec<&str> = segments.iter().map(|s| s.best_text()).collect();
        self.joint_calibrate(&texts)
    }

    fn calibrate_final(&mut self, window: &VadWindow) -> String {
        // The window-level batch re-run heard the whole paragraph in one pass — strictly
        // better context than any per-segment text. Fall back to the segments' best concat.
        let best = window.best_text().into_owned();
        self.joint_calibrate(&[best.as_str()])
    }
}

impl Stage2CalibratorImpl {
    /// The shared core: build the prompt (corrections → hotwords → joint text in the XML
    /// envelope), run the LLM, fall back to the raw text on failure.
    fn joint_calibrate(&mut self, texts: &[&str]) -> String {
        let hotwords = self.hotwords.lock().unwrap().clone();
        let corrections = self.corrections.lock().unwrap().clone();

        let mut pb = PromptBuilder::new_multi(texts).hotwords(&hotwords);
        // User corrections (raw→corrected) — authoritative examples, highest priority.
        if !corrections.is_empty() {
            pb = pb.corrections(&corrections);
        }
        let (system, user) = pb.build();
        tracing::debug!(target: "stage2::prompt", system = %system, user = %user, "calibrate prompt");

        // LLM returns plain text — no JSON parsing. Failure → the raw text, unchanged.
        self.llm.complete(&system, &user).unwrap_or_else(|_| texts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_hotword_store_visible_to_both() {
        // The Stage3→Stage2 feedback channel: the same Arc<Mutex<Vec<String>>> is mutated by
        // Stage3 and read by Stage2. (Calibrator construction needs the real model, exercised
        // in the example; here we just prove the sharing primitive.)
        use std::sync::{Arc, Mutex};
        let store: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec!["Rust".into()]));
        let reader = Arc::clone(&store);
        store.lock().unwrap().push("Bevy".into()); // Stage3 adds
        assert_eq!(*reader.lock().unwrap(), vec!["Rust".to_string(), "Bevy".to_string()]);
    }
}
