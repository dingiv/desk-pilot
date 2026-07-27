//! Dispatcher — holds the engine pieces and implements [`StepEnv`] for the FSM.
//! The stateful composition logic lives in [`state::StateMachine`].

use crate::expander::Expander;
use crate::matcher::Matcher;
use crate::pinyin::InputxPinyin;
use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};
use crate::PinyinEngine;

pub struct Dispatcher {
    matcher: Matcher,
    expander: Expander,
    pinyin: Box<dyn PinyinEngine>,
}

impl Dispatcher {
    pub fn new(matcher: Matcher, expander: Expander) -> Self {
        Dispatcher { matcher, expander, pinyin: Box::new(InputxPinyin::new()) }
    }

    #[cfg(test)]
    pub fn new_for_test(matcher: Matcher, expander: Expander, pinyin: Box<dyn PinyinEngine>) -> Self {
        Dispatcher { matcher, expander, pinyin }
    }

    pub fn process_key(&self, ch: char, sm: &mut StateMachine) -> ImeView {
        sm.step(ch, self)
    }

    pub fn select_candidate(&self, index: usize, sm: &mut StateMachine) -> ImeView {
        sm.select(index)
    }

    pub fn reset(&self, sm: &mut StateMachine) {
        sm.reset();
    }

    pub fn reload_matcher(&mut self, entries: Vec<(String, String)>) {
        self.matcher = Matcher::new(entries);
    }
}

impl StepEnv for Dispatcher {
    fn matcher(&self) -> &Matcher { &self.matcher }
    fn expander(&self) -> &Expander { &self.expander }
    fn pinyin(&self) -> &dyn PinyinEngine { &*self.pinyin }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expander::StaticProvider;

    struct StubPinyin;
    impl PinyinEngine for StubPinyin {
        fn candidates(&self, pinyin: &str) -> Vec<String> {
            match pinyin { "n" => vec!["嗯".into()], "ni" => vec!["你".into(), "呢".into()], _ => Vec::new() }
        }
    }

    fn d() -> Dispatcher {
        let entries = vec![("/greet".into(), "你好,我是 AI 秘书".into()), ("#date".into(), "2026-07-23".into())];
        Dispatcher::new_for_test(Matcher::new(entries), Expander::new(Box::new(StaticProvider { date: "2026-07-23".into(), clipboard: String::new() })), Box::new(StubPinyin))
    }

    fn sm() -> StateMachine { StateMachine::new() }

    #[test]
    fn idle_letter_enters_pinyin() {
        let d = d(); let mut s = sm();
        let v = d.process_key('n', &mut s);
        assert!(v.candidate_count > 0);
        assert_eq!(ImeView::str_field(&v.candidates[0].text), "嗯");
        assert_eq!(s.state, crate::state::ComposeState::Pinyin);
    }

    #[test]
    fn snippet_expansion() {
        let d = d(); let mut s = sm();
        d.process_key('/', &mut s); d.process_key('g', &mut s); d.process_key('r', &mut s); d.process_key('e', &mut s); d.process_key('e', &mut s);
        assert_eq!(ImeView::str_field(&d.process_key('t', &mut s).commit_text), "你好,我是 AI 秘书");
    }

    #[test]
    fn pinyin_space_commits_top() {
        let d = d(); let mut s = sm();
        d.process_key('n', &mut s); d.process_key('i', &mut s);
        assert_eq!(ImeView::str_field(&d.process_key(' ', &mut s).commit_text), "你");
    }

    #[test]
    fn pinyin_enter_commits_raw() {
        let d = d(); let mut s = sm();
        d.process_key('h', &mut s); d.process_key('e', &mut s); d.process_key('l', &mut s); d.process_key('l', &mut s); d.process_key('o', &mut s);
        assert_eq!(ImeView::str_field(&d.process_key('\n', &mut s).commit_text), "hello");
    }

    #[test]
    fn pinyin_and_snippet_coexist() {
        let d = d(); let mut s = sm();
        d.process_key('#', &mut s); d.process_key('d', &mut s); d.process_key('a', &mut s); d.process_key('t', &mut s);
        assert_eq!(ImeView::str_field(&d.process_key('e', &mut s).commit_text), "2026-07-23");
        let a = d.process_key('n', &mut s);
        assert!(a.candidate_count > 0, "after snippet, should enter pinyin, got {a:?}");
    }

    #[test]
    fn select_candidate_commits_nth() {
        let d = d(); let mut s = sm();
        d.process_key('n', &mut s); d.process_key('i', &mut s);
        assert_eq!(ImeView::str_field(&d.select_candidate(1, &mut s).commit_text), "呢");
    }

    #[test]
    fn reset_clears_all() {
        let d = d(); let mut s = sm();
        d.process_key('n', &mut s); d.reset(&mut s);
        assert!(s.buffer.is_empty()); assert_eq!(s.state, crate::state::ComposeState::Idle);
    }
}
