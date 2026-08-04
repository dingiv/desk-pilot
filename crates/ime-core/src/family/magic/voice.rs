//! VoiceMember — `#asr` (alias `#flush`): live voice-input anchor.
//!
//! Activation token `__ASR_BUFFER__`. The candidate list tracks the aura stream:
//! the live interim as #1 while streaming, then settled finals (newest first).
//! Space commits the **full** #1 text; Esc / Enter / Backspace cancel; other keys
//! pass through so typing while listening still works. `tick` rebuilds the view
//! when the shared [`AsrBuffer`] version advances.

use std::sync::Arc;

use super::member::{preview_text, CANDIDATE_PREVIEW_MAX, MagicMember, MemberAction};
use super::MagicResources;
use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};

/// Live voice-input command (`#asr` / `#flush`).
pub struct VoiceMember {
    /// Shared resources — the voice buffer slot is attached late (after engine
    /// construction), so it lives behind an `Arc` shared with the engine.
    resources: Arc<MagicResources>,
    /// Last `AsrBuffer::version()` seen — `tick` compares to detect changes.
    last_version: u64,
    /// Full (un-truncated) texts of the current candidates, parallel to the
    /// display previews in `sm.candidates`. Space commits `full[0]`.
    full: Vec<String>,
}

impl VoiceMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        VoiceMember { resources, last_version: 0, full: Vec::new() }
    }

    /// Rebuild the candidate view from the voice buffer: `[live, finals…]` — the
    /// active utterance is #1, then settled finals newest→oldest. `sm.candidates`
    /// holds previews; `self.full` holds the full texts for commit.
    fn refresh(&mut self, sm: &mut StateMachine) -> ImeView {
        let buf = self.resources.voice.get();
        let mut full: Vec<String> = Vec::new();
        if let Some(buf) = buf.as_ref() {
            let (finals, live) = buf.voice_candidates();
            if !live.is_empty() {
                full.push(live); // active streaming → #1
            }
            full.extend(finals); // then settled, newest→oldest
        }
        let empty = full.is_empty();
        self.full = full;
        if empty {
            // placeholder until voice arrives (non-committable)
            sm.candidates = vec!["语音识别中...".to_string()];
            self.full.clear();
        } else {
            sm.candidates = self.full.iter().map(|t| preview_text(t, CANDIDATE_PREVIEW_MAX)).collect();
        }
        sm.candidates_fresh = true;
        sm.candidate_highlight = 0;
        sm.candidate_page = 0;
        sm.preedit = if empty { "🎙 #asr …".into() } else { "🎙 #asr".into() };
        sm.cursor = sm.preedit.len();
        if let Some(b) = buf.as_ref() {
            self.last_version = b.version();
        }
        tracing::debug!(previews = ?sm.candidates, full = ?self.full, "voice candidates rebuilt");
        sm.make_view()
    }
}

impl MagicMember for VoiceMember {
    fn name(&self) -> &'static str {
        "asr"
    }

    fn description(&self) -> &'static str {
        "voice input"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__ASR_BUFFER__")
    }

    fn aliases(&self) -> &[&'static str] {
        &["flush"]
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(VoiceMember::new(Arc::clone(&self.resources)))
    }

    fn activate(&mut self, sm: &mut StateMachine, _env: &dyn StepEnv) -> ImeView {
        self.refresh(sm)
    }

    fn on_key(&mut self, sm: &mut StateMachine, ch: char, _env: &dyn StepEnv) -> MemberAction {
        match ch {
            ' ' => {
                // Commit the FULL text (self.full), not the display preview (sm.candidates).
                let text = self.full.first().cloned()
                    .unwrap_or_else(|| sm.candidates.first().cloned().unwrap_or_default());
                // Placeholder ("…"/"语音识别中...") or empty → commit nothing.
                if text.is_empty() || text.ends_with("...") {
                    MemberAction::Commit(String::new())
                } else {
                    MemberAction::Commit(text)
                }
            }
            d @ '1'..='9' => {
                let idx = (d as u8 - b'1') as usize;
                match self.full.get(idx) {
                    Some(t) => MemberAction::Commit(t.clone()),
                    // No such candidate — let the application have the digit.
                    None => MemberAction::View(StateMachine::passthrough_view()),
                }
            }
            '\x1b' | '\n' | '\r' | '\x08' => MemberAction::Exit,
            _ => MemberAction::View(StateMachine::passthrough_view()),
        }
    }

    fn tick(&mut self, sm: &mut StateMachine, _env: &dyn StepEnv) -> Option<ImeView> {
        let buf = self.resources.voice.get()?;
        let cur = buf.version();
        if cur == self.last_version {
            // Voice data hasn't advanced since the last rebuild (normal during
            // silence). Trace-level to avoid spam — the timer repeats.
            tracing::trace!(last_version = self.last_version, cur, "voice tick: no version change");
            return None;
        }
        tracing::debug!(last_version = self.last_version, cur, "voice tick rebuild");
        Some(self.refresh(sm))
    }

    fn candidate_texts(&self, sm: &StateMachine) -> Vec<String> {
        if self.full.is_empty() {
            sm.candidates.clone()
        } else {
            self.full.clone()
        }
    }
}

/// `#submit` — one-shot voice snapshot commit. Activates to a single candidate
/// (the latest settled utterance, or a hint if none); Space commits it, Escape
/// cancels.
pub struct SubmitMember {
    resources: Arc<MagicResources>,
}

impl SubmitMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        SubmitMember { resources }
    }
}

impl MagicMember for SubmitMember {
    fn name(&self) -> &'static str {
        "submit"
    }

    fn description(&self) -> &'static str {
        "commit voice snapshot"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__ASR_SUBMIT__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(SubmitMember::new(Arc::clone(&self.resources)))
    }

    fn activate(&mut self, sm: &mut StateMachine, _env: &dyn StepEnv) -> ImeView {
        let text = self.resources.voice.get().map(|b| b.snapshot()).unwrap_or_default();
        sm.candidates = vec![if text.is_empty() { "无语音内容".into() } else { text }];
        sm.candidates_fresh = true;
        sm.candidate_highlight = 0;
        sm.candidate_page = 0;
        sm.preedit = "#submit".into();
        sm.cursor = sm.preedit.len();
        sm.make_view()
    }

    fn on_key(&mut self, sm: &mut StateMachine, ch: char, _env: &dyn StepEnv) -> MemberAction {
        match ch {
            ' ' => {
                let text = sm.candidates.first().cloned().unwrap_or_default();
                if text == "无语音内容" {
                    MemberAction::Commit(String::new())
                } else {
                    MemberAction::Commit(text)
                }
            }
            '\x1b' | '\n' | '\r' | '\x08' => MemberAction::Exit,
            _ => MemberAction::View(StateMachine::passthrough_view()),
        }
    }

    fn tick(&mut self, _sm: &mut StateMachine, _env: &dyn StepEnv) -> Option<ImeView> {
        None
    }
}
