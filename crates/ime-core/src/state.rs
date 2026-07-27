//! IME composition state machine.
//!
//! ## State Transition Table
//!
//! | Current  | Input      | → Next   | View filled                |
//! |----------|------------|----------|----------------------------|
//! | Idle     | `/` `#`    | Snippet  | preedit_text               |
//! | Idle     | a-z        | Pinyin   | candidates or preedit_text  |
//! | Idle     | other      | Idle     | key_passthrough=1          |
//! | Snippet  | letter/dig | Snippet  | trie step → commit/preedit |
//! | Snippet  | dead-end   | Idle     | commit_text                |
//! | Pinyin   | a-z        | Pinyin   | extend + fill_view         |
//! | Pinyin   | Space      | Idle     | commit_text                |
//! | Pinyin   | Enter      | Idle     | commit_text                |
//! | Pinyin   | Backspace  | P/Idle   | pop + fill_view            |
//! | Pinyin   | other      | Idle     | commit_text                |

use crate::expander::Expander;
use crate::matcher::{Match, Matcher};
use crate::platform::{CandidateSlot, CANDIDATE_SLOTS, ImeView};
use crate::PinyinEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposeState { #[default] Idle, Snippet, Pinyin }

#[derive(Debug, Clone, Default)]
pub struct StateMachine {
    pub state: ComposeState,
    pub buffer: String,
    pub preedit: String,
    pub cursor: usize,
    pub candidates: Vec<String>,
    pub candidates_fresh: bool,
    pub candidate_highlight: usize,
    pub candidate_page: usize,
    pub candidate_page_size: usize,
}

impl StateMachine {
    pub fn new() -> Self { StateMachine::default() }

    pub fn step(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        match self.state {
            ComposeState::Idle => self.handle_idle(ch, env),
            ComposeState::Snippet => self.handle_snippet(ch, env),
            ComposeState::Pinyin => self.handle_pinyin(ch, env),
        }
    }

    pub fn select(&mut self, index: usize) -> ImeView {
        let picked = self.candidates.get(index).cloned();
        self.reset();
        Self::commit_view(&picked.unwrap_or_default())
    }

    pub fn reset(&mut self) {
        self.state = ComposeState::Idle;
        self.buffer.clear();
        self.preedit.clear();
        self.cursor = 0;
        self.candidates.clear();
        self.candidate_highlight = 0;
        self.candidate_page = 0;
        self.candidates_fresh = false;
    }

    pub fn move_highlight(&mut self, delta: i32) {
        if self.candidates.is_empty() { return; }
        let new = (self.candidate_highlight as i32 + delta)
            .clamp(0, self.candidates.len() as i32 - 1) as usize;
        self.candidate_highlight = new;
        if self.candidate_page_size > 0 {
            self.candidate_page = new / self.candidate_page_size;
        }
    }

    // ── view helpers ────────────────────────────────────────────────────

    fn fill_view(&self, view: &mut ImeView) {
        ImeView::set_str(&mut view.preedit_text, &self.preedit);
        view.preedit_cursor = self.cursor as u32;
        let n = self.candidates.len().min(CANDIDATE_SLOTS);
        for i in 0..n {
            view.candidates[i] = CandidateSlot::from_str(&self.candidates[i]);
        }
        view.candidate_count = n as u32;
        view.candidate_highlight = self.candidate_highlight as u32;
        view.candidate_page = self.candidate_page as u32;
        view.candidate_page_size = self.candidate_page_size as u32;
        ImeView::set_str(&mut view.aux_up, &self.preedit);
    }

    fn make_view(&self) -> ImeView {
        let mut v = ImeView::empty();
        self.fill_view(&mut v);
        v
    }

    fn commit_view(text: &str) -> ImeView {
        let mut v = ImeView::empty();
        ImeView::set_str(&mut v.commit_text, text);
        v
    }

    fn passthrough_view() -> ImeView {
        let mut v = ImeView::empty();
        v.key_passthrough = 1;
        v
    }

    // ── Idle ───────────────────────────────────────────────────────────

    fn handle_idle(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        if env.matcher().is_trigger_prefix(ch) {
            self.state = ComposeState::Snippet;
            self.buffer.push(ch);
            self.preedit = self.buffer.clone();
            self.cursor = 1;
            return self.make_view();
        }
        if ch.is_ascii_lowercase() {
            self.state = ComposeState::Pinyin;
            self.buffer.push(ch);
            self.preedit = self.buffer.clone();
            self.cursor = self.preedit.len();
            self.candidates_fresh = false;
            return self.query_pinyin(env);
        }
        Self::passthrough_view()
    }

    // ── Snippet ────────────────────────────────────────────────────────

    fn handle_snippet(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        // Backspace: pop the last trigger char. If buffer becomes empty, back to Idle.
        if ch == '\x08' {
            self.buffer.pop();
            if self.buffer.is_empty() {
                // Snippet fully backspaced — consume the key, back to Idle.
                self.reset();
                return ImeView::empty();
            }
            self.preedit = self.buffer.clone();
            self.cursor = self.preedit.len();
            return self.make_view();
        }
        match env.matcher().step(&self.buffer, ch) {
            Match::Complete { expansion, .. } => {
                let expanded = match env.expander().expand(&expansion) {
                    Ok(t) => t,
                    Err(e) => { tracing::warn!(error = %e, "expand failed"); expansion }
                };
                self.reset();
                Self::commit_view(&expanded)
            }
            Match::Partial => {
                self.buffer.push(ch);
                self.preedit = self.buffer.clone();
                self.cursor = self.preedit.len();
                self.make_view()
            }
            Match::None => {
                let mut text = self.buffer.clone();
                text.push(ch);
                self.reset();
                Self::commit_view(&text)
            }
        }
    }

    // ── Pinyin ─────────────────────────────────────────────────────────

    fn handle_pinyin(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        match ch {
            '\x08' => self.pinyin_backspace(env),
            '\n' | '\r' => self.pinyin_enter(),
            ' ' => self.pinyin_space(),
            c if c.is_ascii_lowercase() => {
                self.buffer.push(c);
                self.preedit = self.buffer.clone();
                self.cursor = self.preedit.len();
                self.candidates_fresh = false;
                self.query_pinyin(env)
            }
            c => self.pinyin_terminator(c),
        }
    }

    fn query_pinyin(&mut self, env: &dyn StepEnv) -> ImeView {
        let cands = env.pinyin().candidates(&self.buffer);
        if !cands.is_empty() {
            self.candidates.clone_from(&cands);
            self.candidate_highlight = 0;
            self.candidate_page = 0;
            self.candidates_fresh = true;
        } else {
            // Clear stale candidates from a previous query (e.g. "ni" had
            // candidates, user typed more → "nih" has none → don't show old ones).
            self.candidates.clear();
            self.candidates_fresh = false;
        }
        self.make_view()
    }

    fn pinyin_backspace(&mut self, env: &dyn StepEnv) -> ImeView {
        self.buffer.pop();
        self.preedit = self.buffer.clone();
        self.cursor = self.preedit.len();
        self.candidates_fresh = false;
        if self.buffer.is_empty() {
            // Preedit fully cleared — consume this backspace so it doesn't
            // "spill over" and delete a document character. Return an empty
            // view (no passthrough) so the frontend calls filterAndAccept.
            self.reset();
            ImeView::empty()
        } else {
            self.query_pinyin(env)
        }
    }

    fn pinyin_enter(&mut self) -> ImeView {
        let raw = std::mem::take(&mut self.buffer);
        self.candidates.clear();
        self.state = ComposeState::Idle;
        Self::commit_view(&raw)
    }

    fn pinyin_space(&mut self) -> ImeView {
        let raw = std::mem::take(&mut self.buffer);
        let fresh = self.candidates_fresh;
        self.candidates_fresh = false;
        self.state = ComposeState::Idle;
        if !fresh {
            self.candidates.clear();
            return Self::commit_view(&raw);
        }
        let idx = self.candidate_highlight.min(self.candidates.len().saturating_sub(1));
        let picked = self.candidates.get(idx).cloned();
        self.candidates.clear();
        Self::commit_view(&picked.unwrap_or(raw))
    }

    fn pinyin_terminator(&mut self, ch: char) -> ImeView {
        let fresh = self.candidates_fresh;
        let top = self.candidates.first().cloned();
        let raw = std::mem::take(&mut self.buffer);
        self.candidates_fresh = false;
        self.state = ComposeState::Idle;
        if !fresh {
            self.candidates.clear();
            return Self::commit_view(&format!("{raw}{ch}"));
        }
        self.candidates.clear();
        let text = match top {
            Some(t) => format!("{t}{ch}"),
            None => format!("{raw}{ch}"),
        };
        Self::commit_view(&text)
    }
}

/// Borrowed engine components needed by the FSM to evaluate transitions.
pub trait StepEnv {
    fn matcher(&self) -> &Matcher;
    fn expander(&self) -> &Expander;
    fn pinyin(&self) -> &dyn PinyinEngine;
}
