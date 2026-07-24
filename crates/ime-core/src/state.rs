//! IME composition state machine.
//!
//! ## State Transition Table
//!
//! | Current  | Input      | → Next   | Action                     |
//! |----------|------------|----------|----------------------------|
//! | Idle     | `/` `#`    | Snippet  | Preedit("/")               |
//! | Idle     | a-z        | Pinyin   | candidates() or Preedit    |
//! | Idle     | other      | Idle     | PassThrough                |
//! | Snippet  | letter/dig | Snippet  | trie step → Preedit/Commit |
//! | Snippet  | dead-end   | Idle     | Commit(accumulated+char)   |
//! | Pinyin   | a-z        | Pinyin   | extend buffer + candidates |
//! | Pinyin   | Space      | Idle     | Commit(top_candidate)      |
//! | Pinyin   | Enter      | Idle     | Commit(raw_buffer)         |
//! | Pinyin   | Backspace  | Pinyin   | pop + recandidate          |
//! | Pinyin   | Backspace  | Idle     | pop → empty → PassThrough  |
//! | Pinyin   | other      | Idle     | Commit(top + char) or PassThrough |

use crate::expander::Expander;
use crate::matcher::{Match, Matcher};
use crate::platform::{Candidate, ImeAction};
use crate::PinyinEngine;

/// The three composition states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposeState {
    /// No active composition — waiting for the first character.
    #[default]
    Idle,
    /// Accumulating a snippet trigger (e.g. "/greet", "#date").
    Snippet,
    /// Accumulating pinyin input (e.g. "nihao").
    Pinyin,
}

/// Per-input-context mutable state. Created fresh for each IME context, mutated
/// by [`StateMachine::step`], queried by the C ABI layer for candidates.
#[derive(Debug, Clone, Default)]
pub struct StateMachine {
    pub state: ComposeState,
    /// Raw input typed by the user — the pinyin string ("nihao") or snippet
    /// trigger ("/greet"). This is what the user typed, not what the app sees.
    pub buffer: String,
    /// The preedit text currently displayed in the application's input field.
    /// Updated every time the composition changes, cleared on commit/reset.
    /// Guaranteed to match what `setClientPreedit` was last called with.
    pub preedit: String,
    /// Cursor position within `preedit` (byte offset). Updated alongside preedit.
    pub cursor: usize,
    /// Current hanzi candidates (Pinyin mode only).
    pub candidates: Vec<String>,
}

impl StateMachine {
    pub fn new() -> Self {
        StateMachine::default()
    }

    /// Feed one character to the FSM. Returns the IME action to execute.
    ///
    /// `env` provides the engine pieces (Matcher, Expander, PinyinEngine) that
    /// the Dispatcher owns — the FSM borrows them, it doesn't own them.
    pub fn step(&mut self, ch: char, env: &dyn StepEnv) -> ImeAction {
        match self.state {
            ComposeState::Idle => self.handle_idle(ch, env),
            ComposeState::Snippet => self.handle_snippet(ch, env),
            ComposeState::Pinyin => self.handle_pinyin(ch, env),
        }
    }

    /// User selects a candidate from the popup.
    pub fn select(&mut self, index: usize) -> ImeAction {
        let picked = self.candidates.get(index).cloned();
        self.reset();
        match picked {
            Some(text) => ImeAction::Commit(text),
            None => ImeAction::PassThrough,
        }
    }

    /// Reset to Idle — clears all composition state.
    pub fn reset(&mut self) {
        self.state = ComposeState::Idle;
        self.buffer.clear();
        self.preedit.clear();
        self.cursor = 0;
        self.candidates.clear();
    }

    // ── Idle handlers ─────────────────────────────────────────────────────

    fn handle_idle(&mut self, ch: char, env: &dyn StepEnv) -> ImeAction {
        if env.matcher().is_trigger_prefix(ch) {
            self.state = ComposeState::Snippet;
            self.buffer.push(ch);
            self.preedit = self.buffer.clone();
            self.cursor = 1;
            return ImeAction::Preedit { text: self.preedit.clone(), cursor: 1 };
        }
        if ch.is_ascii_lowercase() {
            self.state = ComposeState::Pinyin;
            self.buffer.push(ch);
            self.preedit = self.buffer.clone();
            self.cursor = self.preedit.len();
            return self.query_pinyin(env);
        }
        ImeAction::PassThrough
    }

    // ── Snippet handlers ──────────────────────────────────────────────────

    fn handle_snippet(&mut self, ch: char, env: &dyn StepEnv) -> ImeAction {
        match env.matcher().step(&self.buffer, ch) {
            Match::Complete { trigger, expansion } => {
                let expanded = match env.expander().expand(&expansion) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = %e, "expansion failed");
                        trigger
                    }
                };
                self.reset();
                ImeAction::Commit(expanded)
            }
            Match::Partial => {
                self.buffer.push(ch);
                self.preedit = self.buffer.clone();
                self.cursor = self.preedit.len();
                ImeAction::Preedit {
                    text: self.preedit.clone(),
                    cursor: self.cursor,
                }
            }
            Match::None => {
                let mut text = self.buffer.clone();
                text.push(ch);
                self.reset();
                ImeAction::Commit(text)
            }
        }
    }

    // ── Pinyin handlers ───────────────────────────────────────────────────

    fn handle_pinyin(&mut self, ch: char, env: &dyn StepEnv) -> ImeAction {
        match ch {
            '\x08' => self.pinyin_backspace(env),
            '\n' | '\r' => self.pinyin_enter(),
            ' ' => self.pinyin_space(),
            c if c.is_ascii_lowercase() => {
                self.buffer.push(c);
                self.preedit = self.buffer.clone();
                self.cursor = self.preedit.len();
                self.query_pinyin(env)
            }
            c => self.pinyin_terminator(c),
        }
    }

    fn query_pinyin(&mut self, env: &dyn StepEnv) -> ImeAction {
        let cands = env.pinyin().candidates(&self.buffer);
        self.candidates.clone_from(&cands);
        if cands.is_empty() {
            ImeAction::Preedit { text: self.preedit.clone(), cursor: self.cursor }
        } else {
            ImeAction::Candidates {
                items: cands.iter().map(|t| Candidate {
                    text: t.clone(), label: String::new(), preview: t.clone(),
                }).collect(),
                selected: 0,
            }
        }
    }

    fn pinyin_backspace(&mut self, env: &dyn StepEnv) -> ImeAction {
        self.buffer.pop();
        self.preedit = self.buffer.clone();
        self.cursor = self.preedit.len();
        if self.buffer.is_empty() {
            self.state = ComposeState::Idle;
            ImeAction::PassThrough
        } else {
            self.query_pinyin(env)
        }
    }

    fn pinyin_enter(&mut self) -> ImeAction {
        let raw = std::mem::take(&mut self.buffer);
        self.candidates.clear();
        self.state = ComposeState::Idle;
        ImeAction::Commit(raw)
    }

    fn pinyin_space(&mut self) -> ImeAction {
        let top = self.candidates.first().cloned();
        self.buffer.clear();
        self.candidates.clear();
        self.state = ComposeState::Idle;
        ImeAction::Commit(top.unwrap_or_default())
    }

    fn pinyin_terminator(&mut self, ch: char) -> ImeAction {
        let top = self.candidates.first().cloned();
        self.buffer.clear();
        self.candidates.clear();
        self.state = ComposeState::Idle;
        match top {
            Some(t) => ImeAction::Commit(format!("{t}{ch}")),
            None => ImeAction::PassThrough,
        }
    }
}

/// Borrowed engine components needed by the FSM to evaluate transitions.
/// Implemented by [`Dispatcher`](crate::Dispatcher) — the FSM struct doesn't
/// own these, it borrows them through this trait.
pub trait StepEnv {
    fn matcher(&self) -> &Matcher;
    fn expander(&self) -> &Expander;
    fn pinyin(&self) -> &dyn PinyinEngine;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expander::StaticProvider;
    use crate::matcher::Matcher;
    use crate::platform::NoopPinyin;

    /// Stub pinyin engine — returns candidates for known pinyin strings only.
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

    /// Test environment. Matcher has /greet, #date snippets; Pinyin is Stub.
    fn test_env() -> TestEnv {
        let entries = vec![
            ("/greet".into(), "Hello!".into()),
            ("#date".into(), "2026-07-23".into()),
        ];
        TestEnv {
            matcher: Matcher::new(entries),
            expander: Expander::new(Box::new(StaticProvider {
                date: "2026-07-23".into(), clipboard: String::new(),
            })),
            pinyin: Box::new(StubPinyin),
        }
    }

    struct TestEnv {
        matcher: Matcher,
        expander: Expander,
        pinyin: Box<dyn PinyinEngine>,
    }

    impl StepEnv for TestEnv {
        fn matcher(&self) -> &Matcher { &self.matcher }
        fn expander(&self) -> &Expander { &self.expander }
        fn pinyin(&self) -> &dyn PinyinEngine { &*self.pinyin }
    }

    #[test]
    fn idle_english_passes_through() {
        let env = test_env();
        let mut sm = StateMachine::new();
        assert_eq!(sm.step('H', &env), ImeAction::PassThrough);
        assert_eq!(sm.state, ComposeState::Idle);
    }

    #[test]
    fn idle_slash_enters_snippet() {
        let env = test_env();
        let mut sm = StateMachine::new();
        assert_eq!(
            sm.step('/', &env),
            ImeAction::Preedit { text: "/".into(), cursor: 1 }
        );
        assert_eq!(sm.state, ComposeState::Snippet);
    }

    #[test]
    fn idle_letter_enters_pinyin() {
        let env = test_env();
        let mut sm = StateMachine::new();
        match sm.step('n', &env) {
            ImeAction::Candidates { items, .. } => assert_eq!(items[0].text, "嗯"),
            other => panic!("expected Candidates, got {other:?}"),
        }
        assert_eq!(sm.state, ComposeState::Pinyin);
    }

    #[test]
    fn snippet_full_expansion() {
        let env = test_env();
        let mut sm = StateMachine::new();
        sm.step('/', &env); sm.step('g', &env); sm.step('r', &env);
        sm.step('e', &env); sm.step('e', &env);
        assert_eq!(sm.step('t', &env), ImeAction::Commit("Hello!".into()));
        assert_eq!(sm.state, ComposeState::Idle);
    }

    #[test]
    fn pinyin_space_commits_top() {
        let env = test_env();
        let mut sm = StateMachine::new();
        sm.step('n', &env);
        sm.step('i', &env);
        assert_eq!(sm.step(' ', &env), ImeAction::Commit("你".into()));
        assert_eq!(sm.state, ComposeState::Idle);
    }

    #[test]
    fn pinyin_enter_commits_raw() {
        let env = test_env();
        let mut sm = StateMachine::new();
        sm.step('h', &env); sm.step('e', &env); sm.step('l', &env);
        sm.step('l', &env); sm.step('o', &env);
        assert_eq!(sm.step('\n', &env), ImeAction::Commit("hello".into()));
    }

    #[test]
    fn pinyin_backspace_to_idle() {
        let env = test_env();
        let mut sm = StateMachine::new();
        sm.step('n', &env);
        assert_eq!(sm.step('\x08', &env), ImeAction::PassThrough);
        assert_eq!(sm.state, ComposeState::Idle);
    }

    #[test]
    fn snippet_and_pinyin_coexist() {
        let env = test_env();
        let mut sm = StateMachine::new();
        // snippet
        sm.step('#', &env); sm.step('d', &env); sm.step('a', &env);
        sm.step('t', &env);
        assert_eq!(sm.step('e', &env), ImeAction::Commit("2026-07-23".into()));
        assert_eq!(sm.state, ComposeState::Idle);
        // pinyin right after
        let a = sm.step('n', &env);
        assert!(matches!(a, ImeAction::Candidates { .. }),
            "after snippet, 'n' should enter pinyin, got {a:?}");
    }

    #[test]
    fn select_candidate() {
        let env = test_env();
        let mut sm = StateMachine::new();
        sm.step('n', &env);
        sm.step('i', &env);
        assert_eq!(sm.select(1), ImeAction::Commit("呢".into()));
    }

    #[test]
    fn reset_clears_all() {
        let env = test_env();
        let mut sm = StateMachine::new();
        sm.step('n', &env); sm.step('i', &env);
        sm.reset();
        assert!(sm.buffer.is_empty());
        assert!(sm.candidates.is_empty());
        assert_eq!(sm.state, ComposeState::Idle);
    }
}
