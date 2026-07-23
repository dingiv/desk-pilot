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
    /// Pinyin-to-hanzi engine (inputx-pinyin in swift-ime; NoopPinyin/stub in tests).
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
            return ImeAction::Preedit {
                text: state.buffer.clone(),
                cursor: state.buffer.len(),
            };
        }

        // ─── Path: Pinyin terminator — a non-letter while in Pinyin mode commits ───
        // Space commits the top candidate (space consumed, standard IME behavior).
        // Any other symbol/punctuation commits the top candidate with the symbol
        // appended ("你好," in one step). Trigger prefixes can't reach here because
        // buffer is non-empty in Pinyin mode.
        if state.mode == InputMode::Pinyin && !ch.is_ascii_lowercase() {
            return self.commit_pinyin(ch, state);
        }

        // ─── Path: Pinyin — small ASCII letters build a pinyin sequence ───
        // Pinyin letters (a-z) never collide with trigger prefixes ('/' '#'), so the
        // two paths stay cleanly separated by their first character.
        if ch.is_ascii_lowercase() {
            return self.pinyin_key(ch, state);
        }

        // ─── Default: English text — pass through ───
        ImeAction::PassThrough
    }

    /// Handle a pinyin keystroke (a-z), or a terminator (space/symbol) while in Pinyin mode.
    fn pinyin_key(&self, ch: char, state: &mut ImeState) -> ImeAction {
        // A letter while already in Pinyin mode just extends the buffer.
        state.mode = InputMode::Pinyin;
        state.buffer.push(ch);

        let cands = self.pinyin.candidates(&state.buffer);
        state.candidates = cands.clone();
        if cands.is_empty() {
            // No candidate yet (partial syllable) — show the pinyin being typed.
            ImeAction::Preedit { text: state.buffer.clone(), cursor: state.buffer.len() }
        } else {
            ImeAction::Candidates {
                items: cands
                    .iter()
                    .map(|t| crate::platform::Candidate {
                        text: t.clone(),
                        label: String::new(),
                        preview: t.clone(),
                    })
                    .collect(),
                selected: 0,
            }
        }
    }

    /// Commit the top pinyin candidate (called on a terminator key in Pinyin mode).
    fn commit_pinyin(&self, ch: char, state: &mut ImeState) -> ImeAction {
        let top = state.candidates.first().cloned();
        state.buffer.clear();
        state.candidates.clear();
        state.mode = InputMode::Normal;
        match top {
            Some(text) if ch == ' ' => ImeAction::Commit(text),
            Some(text) => ImeAction::Commit(format!("{text}{ch}")),
            None => ImeAction::PassThrough,
        }
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

    /// Called by the platform adapter when the user selects a candidate (digit key
    /// or mouse click in the fcitx5 candidate window). Commits that candidate.
    pub fn select_candidate(&self, index: usize, state: &mut ImeState) -> ImeAction {
        debug!(index, "select_candidate");
        let picked = state.candidates.get(index).cloned();
        state.buffer.clear();
        state.candidates.clear();
        state.mode = InputMode::Normal;
        match picked {
            Some(text) => ImeAction::Commit(text),
            None => ImeAction::PassThrough,
        }
    }

    /// Reset the engine state (on focus change, Escape, etc.).
    pub fn reset(&self, state: &mut ImeState) {
        debug!("reset");
        state.buffer.clear();
        state.candidates.clear();
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

    /// Stub pinyin engine returning fixed candidates — keeps ime-core tests
    /// independent of the real inputx-pinyin dictionary.
    struct StubPinyin;
    impl PinyinEngine for StubPinyin {
        fn candidates(&self, pinyin: &str) -> Vec<String> {
            match pinyin {
                "n" => vec!["嗯".into()],
                "ni" => vec!["你".into(), "呢".into()],
                "nihao" => vec!["你好".into()],
                _ => Vec::new(),
            }
        }
    }

    fn dispatcher() -> Dispatcher {
        let store_entries = vec![
            ("/greet".into(), "你好,我是 AI 秘书".into()),
            ("/sig".into(), "Best,\n$DATE".into()),
            ("#asr".into(), "VOICE_BUFFER".into()),
            ("#date".into(), "2026-07-23".into()),
        ];
        let matcher = Matcher::new(store_entries);
        let expander = Expander::new(Box::new(StaticProvider {
            date: "2026-07-23".into(),
            clipboard: "".into(),
        }));
        Dispatcher::new(matcher, expander, Box::new(StubPinyin))
    }

    #[test]
    fn snippet_expansion_still_works() {
        // Pinyin integration must not break snippet matching.
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('/', &mut s);
        d.process_key('g', &mut s);
        d.process_key('r', &mut s);
        d.process_key('e', &mut s);
        d.process_key('e', &mut s);
        assert_eq!(
            d.process_key('t', &mut s),
            ImeAction::Commit("你好,我是 AI 秘书".into())
        );
        assert_eq!(s.mode, InputMode::Normal);
    }

    #[test]
    fn pinyin_letters_produce_candidates() {
        let d = dispatcher();
        let mut s = ImeState::default();
        match d.process_key('n', &mut s) {
            ImeAction::Candidates { items, .. } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].text, "嗯");
            }
            other => panic!("'n' should yield candidates, got {other:?}"),
        }
        assert_eq!(s.mode, InputMode::Pinyin);
        match d.process_key('i', &mut s) {
            ImeAction::Candidates { items, .. } => assert_eq!(items[0].text, "你"),
            other => panic!("'ni' should yield candidates, got {other:?}"),
        }
    }

    #[test]
    fn pinyin_space_commits_top_candidate() {
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert_eq!(d.process_key(' ', &mut s), ImeAction::Commit("你".into()));
        assert!(s.buffer.is_empty());
        assert_eq!(s.mode, InputMode::Normal);
    }

    #[test]
    fn pinyin_punctuation_appends_to_top() {
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert_eq!(d.process_key(',', &mut s), ImeAction::Commit("你,".into()));
    }

    #[test]
    fn select_candidate_commits_nth() {
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s); // candidates: [你, 呢]
        assert_eq!(d.select_candidate(1, &mut s), ImeAction::Commit("呢".into()));
    }

    #[test]
    fn pinyin_and_snippet_coexist() {
        // The core guarantee: snippet and pinyin don't interfere, separated by
        // first character ('#'/'/' vs a-z).
        let d = dispatcher();
        let mut s = ImeState::default();
        // snippet #date → expand
        d.process_key('#', &mut s);
        d.process_key('d', &mut s);
        d.process_key('a', &mut s);
        d.process_key('t', &mut s);
        assert_eq!(d.process_key('e', &mut s), ImeAction::Commit("2026-07-23".into()));
        assert_eq!(s.mode, InputMode::Normal);
        // immediately pinyin after (state must be clean)
        match d.process_key('n', &mut s) {
            ImeAction::Candidates { .. } => {}
            other => panic!("after snippet, 'n' should enter pinyin, got {other:?}"),
        }
        assert_eq!(s.mode, InputMode::Pinyin);
    }

    #[test]
    fn trigger_dead_end_commits_raw() {
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('/', &mut s);
        assert_eq!(d.process_key('x', &mut s), ImeAction::Commit("/x".into()));
        assert_eq!(s.mode, InputMode::Normal);
    }

    #[test]
    fn variable_expansion_in_snippet() {
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('/', &mut s);
        d.process_key('s', &mut s);
        d.process_key('i', &mut s);
        match d.process_key('g', &mut s) {
            ImeAction::Commit(text) => assert!(text.contains("2026-07-23")),
            other => panic!("expected Commit, got {other:?}"),
        }
    }

    #[test]
    fn reset_clears_pinyin_state() {
        let d = dispatcher();
        let mut s = ImeState::default();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert!(!s.candidates.is_empty());
        d.reset(&mut s);
        assert!(s.candidates.is_empty());
        assert_eq!(s.mode, InputMode::Normal);
    }
}
