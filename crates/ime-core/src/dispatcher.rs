//! The middleware dispatch chain — the central routing logic. One `process_key` call
//! per keystroke, returns an [`ImeAction`] that the platform adapter executes.

use tracing::debug;

use crate::expander::Expander;
use crate::matcher::{Match, Matcher};
use crate::platform::{ImeAction, ImeState, InputMode};
use crate::PinyinEngine;

/// The dispatcher holds the composed engine pieces and routes every keystroke.
pub struct Dispatcher {
    matcher: Matcher,
    expander: Expander,
    /// Reserved for Phase 3 (pinyin engine).
    #[allow(dead_code)]
    pinyin: Box<dyn PinyinEngine>,
}

impl Dispatcher {
    /// `pinyin` is the pinyin-to-hanzi engine (use `NoopPinyin` for Phase 1).
    pub fn new(matcher: Matcher, expander: Expander, pinyin: Box<dyn PinyinEngine>) -> Self {
        Dispatcher { matcher, expander, pinyin }
    }

    /// The single entry point for every keystroke. `ch` is the character derived
    /// from the key event (platform adapter does key→char mapping).
    ///
    /// This function is called on the IME framework's main thread and must return
    /// in microseconds — no I/O, no allocations beyond the returned action.
    pub fn process_key(&self, ch: char, state: &mut ImeState) -> ImeAction {
        debug!(ch = %ch, mode = ?state.mode, buffer = %state.buffer, "process_key");

        // ─── Path: Trigger mode (already inside a trigger sequence) ───
        if state.mode == InputMode::Trigger {
            return self.continue_trigger(ch, state);
        }

        // ─── Path: Trigger prefix detected — enter Trigger mode ───
        if self.matcher.is_trigger_prefix(ch) && state.buffer.is_empty() {
            state.mode = InputMode::Trigger;
            state.buffer.push(ch);
            // First char of a trigger is always partial (at least 2 chars needed).
            return ImeAction::Preedit {
                text: state.buffer.clone(),
                cursor: state.buffer.len(),
            };
        }

        // ─── Path: Pinyin mode (reserved for Phase 3) ───
        // In Phase 1, NoopPinyin returns empty → falls through to PassThrough.

        // ─── Default: English text — pass through ───
        ImeAction::PassThrough
    }

    /// Continue an in-progress trigger sequence.
    fn continue_trigger(&self, ch: char, state: &mut ImeState) -> ImeAction {
        match self.matcher.step(&state.buffer, ch) {
            Match::Complete { trigger, expansion } => {
                let expanded = match self.expander.expand(&expansion) {
                    Ok(text) => text,
                    Err(e) => {
                        tracing::warn!(error = %e, "expansion failed, falling back to raw trigger");
                        trigger.clone()
                    }
                };
                state.buffer.clear();
                state.mode = InputMode::Normal;
                debug!(%trigger, %expanded, "trigger expanded");
                ImeAction::Commit(expanded)
            }
            Match::Partial => {
                state.buffer.push(ch);
                // Show preedit so user sees accumulating trigger.
                ImeAction::Preedit {
                    text: state.buffer.clone(),
                    cursor: state.buffer.len(),
                }
            }
            Match::None => {
                // Dead end — this isn't a valid trigger. Reset and pass through
                // the accumulated buffer as committed text (the user typed something
                // starting with '/' that isn't a trigger — just let it through).
                let leftover = {
                    let mut s = state.buffer.clone();
                    s.push(ch);
                    s
                };
                state.buffer.clear();
                state.mode = InputMode::Normal;
                ImeAction::Commit(leftover)
            }
        }
    }

    /// Called by the platform adapter when the user selects a candidate.
    pub fn select_candidate(&self, index: usize, state: &mut ImeState) -> ImeAction {
        debug!(index, "select_candidate");
        // Phase 1: no candidate selection needed (snippets match uniquely).
        // Phase 3: pinyin candidates will use this path.
        let _ = index;
        state.buffer.clear();
        state.mode = InputMode::Normal;
        ImeAction::PassThrough
    }

    /// Reset the engine state (on focus change, Escape, etc.).
    pub fn reset(&self, state: &mut ImeState) {
        debug!("reset");
        state.buffer.clear();
        state.mode = InputMode::Normal;
    }

    /// Rebuild the internal matcher from new snippet data (called after a
    /// hot-reload from familiar config push).
    pub fn reload_matcher(&mut self, entries: Vec<(String, String)>) {
        self.matcher = Matcher::new(entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expander::StaticProvider;
    use crate::platform::NoopPinyin;

    fn dispatcher() -> Dispatcher {
        let store_entries = vec![
            ("/greet".into(), "你好,我是 AI 秘书".into()),
            ("/sig".into(), "Best,\n$DATE".into()),
            ("#asr".into(), "VOICE_BUFFER".into()),
        ];
        let matcher = Matcher::new(store_entries);
        let expander = Expander::new(Box::new(StaticProvider {
            date: "2026-07-23".into(),
            clipboard: "".into(),
        }));
        Dispatcher::new(matcher, expander, Box::new(NoopPinyin))
    }

    #[test]
    fn english_text_passes_through() {
        let d = dispatcher();
        let mut s = ImeState::default();
        assert_eq!(d.process_key('h', &mut s), ImeAction::PassThrough);
        assert_eq!(d.process_key('i', &mut s), ImeAction::PassThrough);
        assert_eq!(s.mode, InputMode::Normal);
    }

    #[test]
    fn full_snippet_expansion() {
        let d = dispatcher();
        let mut s = ImeState::default();

        // '/' enters trigger mode.
        assert_eq!(
            d.process_key('/', &mut s),
            ImeAction::Preedit { text: "/".into(), cursor: 1 }
        );
        assert_eq!(s.mode, InputMode::Trigger);

        // Partial steps.
        assert_eq!(
            d.process_key('g', &mut s),
            ImeAction::Preedit { text: "/g".into(), cursor: 2 }
        );
        d.process_key('r', &mut s);
        d.process_key('e', &mut s);
        d.process_key('e', &mut s);

        // Final 't' completes.
        assert_eq!(
            d.process_key('t', &mut s),
            ImeAction::Commit("你好,我是 AI 秘书".into())
        );
        assert!(s.buffer.is_empty());
        assert_eq!(s.mode, InputMode::Normal);
    }

    #[test]
    fn dead_end_commits_raw_text() {
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('/', &mut s);
        // "/x" doesn't match any trigger — committed immediately on dead end.
        assert_eq!(
            d.process_key('x', &mut s),
            ImeAction::Commit("/x".into())
        );
        assert_eq!(s.mode, InputMode::Normal);
        // Subsequent chars pass through normally.
        assert_eq!(d.process_key('y', &mut s), ImeAction::PassThrough);
    }

    #[test]
    fn variable_expansion_in_snippet() {
        let d = dispatcher();
        let mut s = ImeState::default();
        // "/sig" → "Best,\n$DATE"
        d.process_key('/', &mut s);
        d.process_key('s', &mut s);
        d.process_key('i', &mut s);
        let result = d.process_key('g', &mut s);
        match result {
            ImeAction::Commit(text) => assert!(text.contains("2026-07-23")),
            other => panic!("expected Commit, got {other:?}"),
        }
    }

    #[test]
    fn reset_clears_state() {
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('/', &mut s);
        assert_eq!(s.mode, InputMode::Trigger);
        d.reset(&mut s);
        assert!(s.buffer.is_empty());
        assert_eq!(s.mode, InputMode::Normal);
    }
}
