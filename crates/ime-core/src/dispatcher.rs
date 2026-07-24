//! Dispatcher — holds the engine pieces (Matcher, Expander, PinyinEngine) and
//! implements [`StepEnv`] for the FSM in [`state::StateMachine`]. The FSM owns the
//! composition state; this struct owns the stateless engine components.
//!
//! See [`state`] for the formal state machine definition and transition table.

use crate::expander::Expander;
use crate::matcher::Matcher;
use crate::pinyin::InputxPinyin;
use crate::platform::ImeAction;
use crate::state::{StateMachine, StepEnv};
use crate::PinyinEngine;

/// Holds all stateless engine pieces. The stateful composition logic lives in
/// [`StateMachine`]; this struct just implements [`StepEnv`] so the FSM can
/// borrow the engine during transitions.
pub struct Dispatcher {
    matcher: Matcher,
    expander: Expander,
    pinyin: Box<dyn PinyinEngine>,
}

impl Dispatcher {
    /// Production constructor — uses the built-in `inputx-pinyin` engine.
    pub fn new(matcher: Matcher, expander: Expander) -> Self {
        Dispatcher {
            matcher,
            expander,
            pinyin: Box::new(InputxPinyin::new()),
        }
    }

    /// Test-only constructor with an injectable pinyin engine.
    #[cfg(test)]
    pub fn new_for_test(
        matcher: Matcher,
        expander: Expander,
        pinyin: Box<dyn PinyinEngine>,
    ) -> Self {
        Dispatcher { matcher, expander, pinyin }
    }

    /// The single entry point for every keystroke. The FSM is in `sm`; the
    /// engine pieces are in `self`.
    pub fn process_key(&self, ch: char, sm: &mut StateMachine) -> ImeAction {
        sm.step(ch, self)
    }

    /// User selected a candidate from the popup.
    pub fn select_candidate(&self, index: usize, sm: &mut StateMachine) -> ImeAction {
        sm.select(index)
    }

    /// Reset the per-context composition state (Escape, focus change).
    pub fn reset(&self, sm: &mut StateMachine) {
        sm.reset();
    }

    /// Rebuild the trie from new snippet data (called after hot-reload).
    pub fn reload_matcher(&mut self, entries: Vec<(String, String)>) {
        self.matcher = Matcher::new(entries);
    }
}

impl StepEnv for Dispatcher {
    fn matcher(&self) -> &Matcher {
        &self.matcher
    }
    fn expander(&self) -> &Expander {
        &self.expander
    }
    fn pinyin(&self) -> &dyn PinyinEngine {
        &*self.pinyin
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expander::StaticProvider;
    use crate::state::ComposeState;

    /// Stub pinyin engine for tests — returns candidates for known strings only.
    struct StubPinyin;
    impl PinyinEngine for StubPinyin {
        fn candidates(&self, pinyin: &str) -> Vec<String> {
            match pinyin {
                "n" => vec!["嗯".into()],
                "ni" => vec!["你".into(), "呢".into()],
                _ => Vec::new(),
            }
        }
    }

    fn dispatcher() -> Dispatcher {
        let store_entries = vec![
            ("/greet".into(), "你好,我是 AI 秘书".into()),
            ("/sig".into(), "Best,\n$DATE".into()),
            ("#date".into(), "2026-07-23".into()),
        ];
        let matcher = Matcher::new(store_entries);
        let expander = Expander::new(Box::new(StaticProvider {
            date: "2026-07-23".into(),
            clipboard: "".into(),
        }));
        Dispatcher::new_for_test(matcher, expander, Box::new(StubPinyin))
    }

    fn sm() -> StateMachine {
        StateMachine::new()
    }

    #[test]
    fn idle_letter_enters_pinyin() {
        let mut d = dispatcher();
        let mut s = sm();
        match d.process_key('n', &mut s) {
            ImeAction::Candidates { items, .. } => assert_eq!(items[0].text, "嗯"),
            other => panic!("expected Candidates, got {other:?}"),
        }
        assert_eq!(s.state, ComposeState::Pinyin);
    }

    #[test]
    fn snippet_expansion() {
        let mut d = dispatcher();
        let mut s = sm();
        d.process_key('/', &mut s);
        d.process_key('g', &mut s); d.process_key('r', &mut s);
        d.process_key('e', &mut s); d.process_key('e', &mut s);
        assert_eq!(
            d.process_key('t', &mut s),
            ImeAction::Commit("你好,我是 AI 秘书".into())
        );
        assert_eq!(s.state, ComposeState::Idle);
    }

    #[test]
    fn pinyin_space_commits_top() {
        let mut d = dispatcher();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert_eq!(d.process_key(' ', &mut s), ImeAction::Commit("你".into()));
    }

    #[test]
    fn pinyin_enter_commits_raw() {
        let mut d = dispatcher();
        let mut s = sm();
        d.process_key('h', &mut s); d.process_key('e', &mut s);
        d.process_key('l', &mut s); d.process_key('l', &mut s);
        d.process_key('o', &mut s);
        assert_eq!(d.process_key('\n', &mut s), ImeAction::Commit("hello".into()));
    }

    #[test]
    fn pinyin_and_snippet_coexist() {
        let mut d = dispatcher();
        let mut s = sm();
        d.process_key('#', &mut s); d.process_key('d', &mut s);
        d.process_key('a', &mut s); d.process_key('t', &mut s);
        assert_eq!(d.process_key('e', &mut s), ImeAction::Commit("2026-07-23".into()));
        let a = d.process_key('n', &mut s);
        assert!(matches!(a, ImeAction::Candidates { .. }),
            "after snippet, should enter pinyin, got {a:?}");
    }

    #[test]
    fn select_candidate_commits_nth() {
        let mut d = dispatcher();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert_eq!(d.select_candidate(1, &mut s), ImeAction::Commit("呢".into()));
    }

    #[test]
    fn reset_clears_all() {
        let mut d = dispatcher();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.reset(&mut s);
        assert!(s.buffer.is_empty());
        assert_eq!(s.state, ComposeState::Idle);
    }
}
