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
//!
//! ## Incremental composition (造词)
//!
//! When the buffer contains 2+ syllables, the candidate list shows BOTH:
//!  - Full Viterbi compositions (select → commit entire word)
//!  - First-syllable single characters (select → commit that char, reduce buffer)
//!
//! After each partial commit the buffer shrinks and the query repeats.
//! When the last syllable is committed, the resulting phrase is saved to
//! the PhraseBook for future sessions.

use crate::expander::Expander;
use crate::matcher::{Match, Matcher};
use crate::platform::{CANDIDATE_SLOTS, ImeView};
use crate::PinyinEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposeState { #[default] Idle, Snippet, Pinyin }

#[derive(Debug, Clone, Default)]
pub struct StateMachine {
    pub state: ComposeState,
    /// Raw pinyin buffer — remaining uncommitted pinyin syllables.
    pub buffer: String,
    /// Visual preedit: committed hanzi + remaining pinyin.
    pub preedit: String,
    /// Cursor byte offset within preedit.
    pub cursor: usize,
    pub candidates: Vec<String>,
    pub candidates_fresh: bool,
    pub candidate_highlight: usize,
    pub candidate_page: usize,
    pub candidate_page_size: usize,
    /// Hanzi already committed during incremental composition (e.g. "李正").
    pub committed_text: String,
    /// Pinyin corresponding to the committed hanzi (e.g. "lizheng").
    committed_pinyin_buf: String,
    /// How many of the first candidates are full-word compositions
    /// (for backward compat and UI display offset).
    pub full_comp_count: usize,
    /// Indices in `candidates` that are single-char partial-commit options.
    partial_commit_indices: Vec<bool>,
    /// Short-term input context — accumulates recently committed text.
    pub context: crate::family::InputContext,
    /// Pending snippet/magic expansion text. When set, the expansion is
    /// shown as a candidate rather than auto-committed. Space/digit to
    /// commit, Enter to force raw text.
    pending_expansion: Option<String>,
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

    /// Select candidate at `index`.
    ///
    /// Full commit (`index < full_comp_count`): commits everything, records
    /// the pick in inputx-pinyin's L0 user model for frequency boosting.
    /// Multi-step compositions also save to the PhraseBook for recall.
    ///
    /// Partial commit (`index >= full_comp_count`): appends the single
    /// character to [`committed_text`], shrinks the buffer by one syllable,
    /// and re-queries. The character pick is also recorded in L0.
    pub fn select(&mut self, index: usize, env: &dyn StepEnv) -> ImeView {
        let picked = self.candidates.get(index).cloned().unwrap_or_default();
        if picked.is_empty() { return self.make_view(); }

        let is_partial = self.partial_commit_indices.get(index).copied().unwrap_or(false);
        if !is_partial {
            // Full commit: combine committed_text + selected text.
            let final_text = if self.committed_text.is_empty() {
                picked.clone()
            } else {
                format!("{}{}", self.committed_text, picked)
            };
            // Boost this word in inputx-pinyin's L0 user model.
            let full_pinyin = if self.committed_text.is_empty() {
                self.buffer.clone()
            } else {
                format!("{}{}", self.committed_pinyin(), self.buffer)
            };
            env.record_pick(&full_pinyin, &picked);
            // Always save to PhraseBook — L0 only boosts words already in
            // the dictionary; Viterbi-composed words need PhraseBook recall.
            env.learn_phrase(&full_pinyin, &final_text);
            self.context.update(&final_text);
            self.reset();
            Self::commit_view(&final_text)
        } else {
            // Partial commit: append this single character, shrink buffer.
            self.committed_text.push_str(&picked);
            let first_syl = env.first_syllable(&self.buffer).unwrap_or_default();
            let first_len = first_syl.len();
            if first_len > 0 && first_len <= self.buffer.len() {
                // Record this single-char pick in L0.
                let consumed = self.buffer[..first_len].to_string();
                env.record_pick(&consumed, &picked);
                self.committed_pinyin_buf.push_str(&consumed);
                self.buffer = self.buffer[first_len..].to_string();
            }
            self.preedit = format!("{}{}", self.committed_text, self.buffer);
            self.cursor = self.preedit.len();
            self.candidates_fresh = false;
            self.candidate_highlight = 0;
            self.query_pinyin(env)
        }
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
        self.committed_text.clear();
        self.committed_pinyin_buf.clear();
        self.full_comp_count = 0;
        self.partial_commit_indices.clear();
        self.pending_expansion = None;
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

    /// Full pinyin for the committed portion.
    fn committed_pinyin(&self) -> String {
        self.committed_pinyin_buf.clone()
    }

    // ── view helpers ────────────────────────────────────────────────────

    fn fill_view(&self, view: &mut ImeView) {
        ImeView::set_str(&mut view.preedit_text, &self.preedit);
        view.preedit_cursor = self.cursor as u32;
        let n = self.candidates.len().min(CANDIDATE_SLOTS);
        for i in 0..n {
            ImeView::set_str(&mut view.candidates[i].text, &self.candidates[i]);
            // Mark single-char partial-commit candidates with ">" label.
            if self.partial_commit_indices.get(i).copied().unwrap_or(false) {
                ImeView::set_str(&mut view.candidates[i].label, ">");
            }
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
        // Backspace: pop last char from trigger.
        if ch == '\x08' {
            self.buffer.pop();
            self.pending_expansion = None;
            if self.buffer.is_empty() {
                self.reset();
                return ImeView::empty();
            }
            self.preedit = self.buffer.clone();
            self.cursor = self.preedit.len();
            return self.make_view();
        }

        // Enter: force raw text, ignore any pending expansion.
        if ch == '\n' || ch == '\r' {
            let raw = std::mem::take(&mut self.buffer);
            self.reset();
            return Self::commit_view(&raw);
        }

        // Space: commit the pending expansion if one exists, otherwise
        // commit the raw trigger text.
        if ch == ' ' {
            if let Some(expansion) = self.pending_expansion.take() {
                let expanded = match env.expander().expand(&expansion) {
                    Ok(t) => t,
                    Err(e) => { tracing::warn!(error = %e, "expand failed"); expansion }
                };
                self.reset();
                return Self::commit_view(&expanded);
            }
            let raw = std::mem::take(&mut self.buffer);
            self.reset();
            return Self::commit_view(&raw);
        }

        match env.matcher().step(&self.buffer, ch) {
            Match::Complete { expansion, .. } => {
                // Store the expansion as a pending candidate — don't auto-expand.
                self.buffer.push(ch);
                self.preedit = self.buffer.clone();
                self.cursor = self.preedit.len();
                // Show expansion as candidate.
                let expanded = match env.expander().expand(&expansion) {
                    Ok(t) => t,
                    Err(e) => { tracing::warn!(error = %e, "expand failed"); expansion.clone() }
                };
                self.pending_expansion = Some(expansion);
                self.candidates = vec![expanded];
                self.candidates_fresh = true;
                self.candidate_highlight = 0;
                self.full_comp_count = 1;
                self.partial_commit_indices = vec![false];
                self.make_view()
            }
            Match::Partial => {
                self.buffer.push(ch);
                self.preedit = self.buffer.clone();
                self.cursor = self.preedit.len();
                self.pending_expansion = None;
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
            ' ' => self.pinyin_space(env),
            c if c.is_ascii_lowercase() => {
                self.buffer.push(c);
                self.preedit = format!("{}{}", self.committed_text, self.buffer);
                self.cursor = self.preedit.len();
                self.candidates_fresh = false;
                self.query_pinyin(env)
            }
            c => self.pinyin_terminator(c),
        }
    }

    fn query_pinyin(&mut self, env: &dyn StepEnv) -> ImeView {
        // Unified scorer: collects candidates from all enabled families,
        // ranks them by weighted score, returns deduplicated text list.
        let cands = env.scorer().rank_with_context(&self.buffer, &self.context);

        let mut merged = Vec::new();
        self.partial_commit_indices.clear();

        // Layer 3: if buffer has 2+ syllables, add first-syllable single-char
        // options for incremental composition (造词).
        // Interleave: a few top full comps, then single-char options.
        if let Some(first_syl) = env.first_syllable(&self.buffer) {
            if first_syl.len() < self.buffer.len() {
                let max_full = 6usize; // show at most 6 full comps on first page
                let char_cands: Vec<String> = env.pinyin().candidates(&first_syl)
                    .into_iter()
                    .filter(|c| c.chars().count() == 1 && !merged.contains(c))
                    .take(CANDIDATE_SLOTS)
                    .collect();

                if !char_cands.is_empty() {
                    // Top full comps.
                    let full_head = cands.iter().take(max_full).cloned()
                        .collect::<Vec<_>>();
                    self.full_comp_count = full_head.len();
                    for _ in 0..full_head.len() {
                        self.partial_commit_indices.push(false);
                    }
                    merged.extend(full_head);

                    // Single-char options (labeled ">").
                    for _ in 0..char_cands.len() {
                        self.partial_commit_indices.push(true);
                    }
                    merged.extend(char_cands);

                    // Remaining full comps.
                    let full_tail: Vec<String> = cands.iter()
                        .skip(max_full).cloned()
                        .filter(|c| !merged.contains(c))
                        .collect();
                    for _ in 0..full_tail.len() {
                        self.partial_commit_indices.push(false);
                    }
                    merged.extend(full_tail);
                } else {
                    self.full_comp_count = cands.len();
                    for _ in 0..cands.len() {
                        self.partial_commit_indices.push(false);
                    }
                    merged = cands;
                }
            } else {
                self.full_comp_count = cands.len();
                for _ in 0..cands.len() {
                    self.partial_commit_indices.push(false);
                }
                merged = cands;
            }
        } else {
            self.full_comp_count = cands.len();
            for _ in 0..cands.len() {
                self.partial_commit_indices.push(false);
            }
            merged = cands;
        }

        let cands = merged;

        if !cands.is_empty() {
            self.candidates.clone_from(&cands);
            self.candidate_highlight = 0;
            self.candidate_page = 0;
            self.candidates_fresh = true;
        } else {
            self.candidates.clear();
            self.candidates_fresh = false;
        }
        self.make_view()
    }

    fn pinyin_backspace(&mut self, env: &dyn StepEnv) -> ImeView {
        // If we have committed text, backspace undoes the last committed char.
        if !self.committed_text.is_empty() {
            self.committed_text.pop();
            // Undo the last consumed syllable from committed_pinyin_buf.
            let last_syl = env.first_syllable(&self.committed_pinyin_buf);
            if let Some(syl) = last_syl {
                let trim = self.committed_pinyin_buf.len().saturating_sub(syl.len());
                self.committed_pinyin_buf.truncate(trim);
                // Prepend the syllable back to buffer.
                self.buffer = format!("{syl}{}", self.buffer);
            }
            self.preedit = format!("{}{}", self.committed_text, self.buffer);
            self.cursor = self.preedit.len();
            self.candidates_fresh = false;
            return self.query_pinyin(env);
        }

        self.buffer.pop();
        self.preedit = format!("{}{}", self.committed_text, self.buffer);
        self.cursor = self.preedit.len();
        self.candidates_fresh = false;
        if self.buffer.is_empty() {
            self.reset();
            ImeView::empty()
        } else {
            self.query_pinyin(env)
        }
    }

    fn pinyin_enter(&mut self) -> ImeView {
        let raw = std::mem::take(&mut self.buffer);
        let committed = std::mem::take(&mut self.committed_text);
        let text = if committed.is_empty() { raw } else { format!("{committed}{raw}") };
        self.reset();
        Self::commit_view(&text)
    }

    fn pinyin_space(&mut self, env: &dyn StepEnv) -> ImeView {
        if !self.candidates_fresh {
            // No candidates — commit raw (committed_text + buffer).
            let committed = std::mem::take(&mut self.committed_text);
            let raw = std::mem::take(&mut self.buffer);
            self.candidates.clear();
            self.state = ComposeState::Idle;
            let text = if committed.is_empty() { raw } else { format!("{committed}{raw}") };
            self.candidates_fresh = false;
            return Self::commit_view(&text);
        }

        // Fresh candidates: commit the highlighted one.
        let idx = self.candidate_highlight.min(self.candidates.len().saturating_sub(1));
        // Delegate to select() — it handles full vs partial commit correctly.
        self.candidates_fresh = false;
        self.select(idx, env)
    }

    fn pinyin_terminator(&mut self, ch: char) -> ImeView {
        let fresh = self.candidates_fresh;
        let top = self.candidates.first().cloned();
        let committed = std::mem::take(&mut self.committed_text);
        let raw = std::mem::take(&mut self.buffer);
        self.candidates_fresh = false;
        self.state = ComposeState::Idle;
        self.candidates.clear();

        let prefix = if committed.is_empty() { String::new() } else { committed };
        if !fresh {
            return Self::commit_view(&format!("{prefix}{raw}{ch}"));
        }
        let text = match top {
            Some(t) => format!("{prefix}{t}{ch}"),
            None => format!("{prefix}{raw}{ch}"),
        };
        Self::commit_view(&text)
    }
}

/// Borrowed engine components needed by the FSM to evaluate transitions.
pub trait StepEnv {
    fn matcher(&self) -> &Matcher;
    fn expander(&self) -> &Expander;
    fn pinyin(&self) -> &dyn PinyinEngine;

    /// Unified candidate scorer — combines all families.
    fn scorer(&self) -> &crate::family::UnifiedScorer;

    /// Extract the first valid pinyin syllable from the input.
    fn first_syllable(&self, pinyin: &str) -> Option<String>;

    /// Record a user pick in inputx-pinyin's L0 layer for frequency boosting.
    fn record_pick(&self, pinyin: &str, word: &str);

    /// Called after a multi-step composition completes.
    fn learn_phrase(&self, pinyin: &str, hanzi: &str);
}
