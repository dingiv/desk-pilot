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
use crate::family::magic::{MagicMember, MemberAction};
use crate::matcher::{Match, Matcher};
use crate::platform::{CANDIDATE_SLOTS, ImeView};
use crate::PinyinEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposeState { #[default] Idle, Snippet, Pinyin, Magic }

#[derive(Default)]
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
    /// Magic command prediction hints while typing `#…`: all commands whose trigger extends
    /// the buffer, as `(trigger, activation_token?)` — live members carry their token (Space
    /// on the hint COMPLETES into that command's Magic mode), static commands carry `None`
    /// (Space resolves their expansion). The raw buffer is the LAST rollback candidate.
    /// Only set in Snippet state; cleared on any other transition.
    magic_hints: Vec<(String, Option<String>)>,
    /// Preview-state candidate tail. In Magic (preview) mode the candidate panel is assembled
    /// in three segments: [member candidates…] [`magic_tail`]…] + the final rollback (the raw
    /// trigger, e.g. `#asr`). `magic_member_cand_count` is where the member segment ends;
    /// `magic_tail` holds the family-prediction continuations (`#asr` → `#asrplus`) with their
    /// activation tokens (None = static expansion). Space routes by highlight: member segment
    /// → the member; a tail continuation → switch into that command's preview; the rollback
    /// → commit the raw trigger text. Only set in Magic state.
    magic_member_cand_count: usize,
    magic_tail: Vec<(String, Option<String>)>,
    /// The active live magic command (`#asr` voice anchor, `#req` HTTP request, …).
    /// `Some` only while in [`Magic`](ComposeState::Magic) state. Each activation
    /// spawns a fresh instance from the [`MagicFamily`] registry.
    pub magic_member: Option<Box<dyn MagicMember>>,
    /// 调试模式:候选词显示提供者与权重(`[score family/source]`)。
    pub candidate_meta_enabled: bool,
    /// 最近一次排名的详细结果(score, family, source)——与 candidates 对齐,
    /// 供 fill_view 填充 meta。
    last_meta: Vec<(f64, &'static str, &'static str)>,
}

impl StateMachine {
    /// 最近一次排名的 (score, family, source)(engine.view 的调试视图用)。
    pub fn last_meta(&self) -> &[(f64, &'static str, &'static str)] {
        &self.last_meta
    }
}

impl std::fmt::Debug for StateMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateMachine")
            .field("state", &self.state)
            .field("buffer", &self.buffer)
            .field("preedit", &self.preedit)
            .field("cursor", &self.cursor)
            .field("candidates", &self.candidates)
            .field("candidate_highlight", &self.candidate_highlight)
            .field(
                "magic_member",
                &self.magic_member.as_ref().map(|m| m.name()),
            )
            .finish()
    }
}

impl StateMachine {
    pub fn new() -> Self {
        StateMachine::with_page_size(7)
    }

    /// Construct with a configurable candidate page size (default 7).
    /// The engine passes `swift-ime.yaml → input.page_size` via
    /// [`ImeEngine::set_page_size`](crate::engine::ImeEngine::set_page_size).
    pub fn with_page_size(page_size: u32) -> Self {
        StateMachine { candidate_page_size: page_size.max(1) as usize, ..StateMachine::default() }
    }

    pub fn step(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        match self.state {
            ComposeState::Idle => self.handle_idle(ch, env),
            ComposeState::Snippet => self.handle_snippet(ch, env),
            ComposeState::Pinyin => self.handle_pinyin(ch, env),
            ComposeState::Magic => self.handle_magic(ch, env),
        }
    }

    /// Magic (`#`-command live mode / preview): the candidate panel is the member's own
    /// candidates followed by the family-prediction tail + rollback (see [`magic_tail`]).
    /// Space routes by highlight: the member segment → the member; a tail continuation
    /// (`#asrplus`) → switch into that command's preview; the rollback → commit the raw
    /// trigger text. Other keys route to the member.
    fn handle_magic(&mut self, ch: char, env: &dyn StepEnv) -> ImeView {
        // Space on the tail segment (continuation / rollback) is handled here, NOT by the
        // member — the member only owns its own candidates.
        if ch == ' ' {
            let hl = self.candidate_highlight;
            if hl >= self.magic_member_cand_count {
                let tail_idx = hl - self.magic_member_cand_count;
                if tail_idx < self.magic_tail.len() {
                    let (trigger, token) = self.magic_tail[tail_idx].clone();
                    // Continuation (`#asrplus`): switch into that command's preview.
                    if let Some(tok) = token {
                        if let Some(new_member) = env.magic().spawn(&tok) {
                            let mut new_member = new_member;
                            self.magic_member.take(); // drop old member (deactivate)
                            self.buffer = trigger.clone();
                            self.pending_expansion = None;
                            self.state = ComposeState::Magic;
                            let view = new_member.activate(self, env);
                            self.magic_member = Some(new_member);
                            self.assemble_magic_tail(env);
                            return view;
                        }
                    }
                    // Static continuation or rollback fallback: commit the trigger text.
                    // (Rollback is the LAST tail entry — commit the raw trigger.)
                    return self.commit_magic_rollback(&trigger);
                }
                // tail_idx == magic_tail.len() → the rollback (raw buffer).
                let raw = self.buffer.clone();
                return self.commit_magic_rollback(&raw);
            }
        }
        let Some(mut member) = self.magic_member.take() else {
            self.reset();
            return ImeView::empty();
        };
        
        match member.on_key(self, ch, env) {
            MemberAction::View(view) => {
                self.magic_member = Some(member);
                self.assemble_magic_tail(env);
                *view
            }
            MemberAction::Commit(text) => {
                member.deactivate();
                self.reset();
                Self::commit_view(&text)
            }
            MemberAction::Exit => {
                member.deactivate();
                self.reset();
                ImeView::empty()
            }
        }
    }

    /// Commit the raw trigger text (`#asr`) as a rollback: deactivate the member, leave
    /// Magic mode, 上屏. (The buffer holds the trigger during preview.)
    fn commit_magic_rollback(&mut self, trigger: &str) -> ImeView {
        if let Some(mut m) = self.magic_member.take() {
            m.deactivate();
        }
        let text = trigger.to_string();
        self.reset();
        Self::commit_view(&text)
    }

    /// Assemble the preview candidate panel: member candidates + family-prediction tail
    /// (continuations like `#asrplus`) + final rollback (the raw trigger). Called after the
    /// member rebuilds its candidates (activate / refresh / tick).
    pub(crate) fn assemble_magic_tail(&mut self, env: &dyn StepEnv) {
        if self.state != ComposeState::Magic {
            return;
        }
        self.magic_member_cand_count = self.candidates.len();
        // Continuations: all commands whose trigger strictly extends the current one.
        self.magic_tail = env.magic()
            .hints(&self.buffer)
            .into_iter()
            .map(|(t, tok)| (t, tok.map(|s| s.to_string())))
            .collect();
        // Panel = [member…] + [tail…] + [rollback].
        let mut cands = self.candidates.clone();
        for (t, _) in &self.magic_tail {
            cands.push(t.clone());
        }
        cands.push(self.buffer.clone()); // rollback
        self.candidates = cands;
        self.candidates_fresh = true;
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
            // Record the FULL composed word, not just the last character.
            env.record_pick(&full_pinyin, &final_text);
            // 自生词模式(经历过 ≥1 次数字键逐字选择,committed_text 非空):
            // 主动造词成果无条件加入单词本 —— 不因词典里恰好有该词而跳过。
            // 直接提交(未逐字选择)走 learn_phrase(词典词不进单词本)。
            if self.committed_text.is_empty() {
                env.learn_phrase(&full_pinyin, &final_text);
            } else {
                env.learn_composed_phrase(&full_pinyin, &final_text);
            }
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
        // Drop the active magic member (after deactivate) — its per-session state
        // goes with it; shared resources (voice slot, req fetcher) survive via Arc.
        if let Some(mut m) = self.magic_member.take() {
            m.deactivate();
        }
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
        self.magic_hints.clear();
        self.magic_member_cand_count = 0;
        self.magic_tail.clear();
    }

    /// Is the candidate panel OPEN (non-empty candidate list)? Navigation/paging special keys
    /// only act while it's open; when closed they pass through to the application.
    pub fn candidate_panel_open(&self) -> bool {
        !self.candidates.is_empty()
    }

    pub fn move_highlight(&mut self, delta: i32) {
        if self.candidates.is_empty() { return; }
        let new = (self.candidate_highlight as i32 + delta)
            .clamp(0, self.candidates.len() as i32 - 1) as usize;
        self.candidate_highlight = new;
        if self.candidate_page_size > 0 {
            self.candidate_page = (new as u32)
                .checked_div(self.candidate_page_size as u32)
                .unwrap_or(0) as usize;
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
            // 调试模式:候选词后附提供者与权重。
            if self.candidate_meta_enabled {
                if let Some((score, fam, src)) = self.last_meta.get(i) {
                    let meta = format!("[{score:.3} {fam}/{src}]");
                    ImeView::set_str(&mut view.candidates[i].meta, &meta);
                }
            }
        }
        view.candidate_count = n as u32;
        view.candidate_highlight = self.candidate_highlight as u32;
        view.candidate_page = self.candidate_page as u32;
        view.candidate_page_size = self.candidate_page_size as u32;
        ImeView::set_str(&mut view.aux_up, &self.preedit);
    }

    /// Build a view from the current state (no key processed). Used by the state
    /// machine itself and by magic members rendering their candidates.
    pub(crate) fn make_view(&self) -> ImeView {
        let mut v = ImeView::empty();
        self.fill_view(&mut v);
        v
    }

    pub(crate) fn commit_view(text: &str) -> ImeView {
        // Default: caret at the end of the committed text.
        let mut v = ImeView::empty();
        ImeView::set_str(&mut v.commit_text, text);
        v.commit_cursor = ImeView::str_field(&v.commit_text).len() as u32;
        v
    }

    /// Commit with the application caret placed at `cursor` (byte offset into the
    /// committed text) — snippet templates with `$CURSOR` land here. Clamped to
    /// the actually-committed length (the buffer may truncate long text).
    pub(crate) fn commit_view_at(text: &str, cursor: usize) -> ImeView {
        let mut v = ImeView::empty();
        ImeView::set_str(&mut v.commit_text, text);
        let len = ImeView::str_field(&v.commit_text).len();
        v.commit_cursor = cursor.min(len) as u32;
        v
    }

    /// View that passes the current key through to the application untouched.
    pub(crate) fn passthrough_view() -> ImeView {
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
            self.magic_hints.clear();
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

        // Space: the highlighted candidate decides. A magic hint (not the last rollback)
        // COMPLETES into that command — live → enter its Magic mode (`#as` + Space behaves
        // like typing `#asr`); static → resolve its expansion. The rollback (last candidate,
        // the raw `#xxx`) and any pending expansion commit text. Preview placeholders (tokens
        // resolving to text ending with "...") commit empty.
        if ch == ' ' {
            let hl = self.candidate_highlight;
            if hl < self.magic_hints.len() {
                let (trigger, token) = self.magic_hints[hl].clone();
                self.magic_hints.clear();
                if let Some(tok) = token {
                    self.buffer = trigger.clone();
                    self.pending_expansion = None;
                    if let Some(member) = env.magic().spawn(&tok) {
                        let mut member = member;
                        self.state = ComposeState::Magic;
                        let view = member.activate(self, env);
                        self.magic_member = Some(member);
                        self.assemble_magic_tail(env);
                        return view;
                    }
                    // Token vanished (registry changed?) — fall through to commit raw.
                    self.buffer = trigger;
                } else {
                    // Static command hint (`#date`) — resolve its expansion inline.
                    let expanded = env.magic().static_expansion(&trigger)
                        .unwrap_or_else(|| trigger.clone());
                    self.reset();
                    if expanded.ends_with("...") {
                        return Self::commit_view("");
                    }
                    return Self::commit_view(&expanded);
                }
            }
            self.magic_hints.clear();
            if let Some(expansion) = self.pending_expansion.take() {
                // Expand tracking the `$CURSOR` marker — the caret lands at its
                // position in the RESULT (variables before it may vary in length).
                let (expanded, cursor) = match env.expander().expand_with_cursor(&expansion) {
                    Ok(t) => t,
                    Err(e) => { tracing::warn!(error = %e, "expand failed"); (expansion, None) }
                };
                self.reset();
                // Preview candidates ("语音识别中...") commit empty.
                if expanded.ends_with("...") {
                    return Self::commit_view("");
                }
                return match cursor {
                    Some(pos) => Self::commit_view_at(&expanded, pos),
                    None => Self::commit_view(&expanded),
                };
            }
            let raw = std::mem::take(&mut self.buffer);
            self.reset();
            return Self::commit_view(&raw);
        }

        match env.matcher().step(&self.buffer, ch) {
            Match::Complete { trigger, expansion } => {
                self.buffer.push(ch);
                // The trigger is fully matched — stale prefix hints from earlier Partial steps
                // must not linger (a later Space would misroute to the hint branch).
                self.magic_hints.clear();
                // Live magic commands (e.g. `#asr`, `#req`) enter Magic mode — the registry
                // spawns a member instance that owns the interactive session (keys + async
                // ticks are routed to it). Static expansions go the pending-candidate path.
                if let Some(member) = env.magic().spawn(&expansion) {
                    let mut member = member;
                    self.state = ComposeState::Magic;
                    self.pending_expansion = None;
                    let view = member.activate(self, env);
                    self.magic_member = Some(member);
                    self.assemble_magic_tail(env);
                    return view;
                }
                // Magic static commands carry a sentinel (expansion == trigger)
                // instead of a frozen value — resolve FRESH so a #date typed
                // days after engine start commits TODAY, not the startup date.
                // User snippets (expansion ≠ trigger) keep their own text.
                let static_expanded = if expansion == trigger {
                    env.magic().static_expansion(&trigger)
                        .unwrap_or_else(|| expansion.clone())
                } else {
                    expansion.clone()
                };
                // Store the expansion as a pending candidate — don't auto-expand.
                self.preedit = self.buffer.clone();
                self.cursor = self.preedit.len();
                let expanded = match env.expander().expand(&static_expanded) {
                    Ok(t) => t,
                    Err(e) => { tracing::warn!(error = %e, "expand failed"); static_expanded.clone() }
                };
                self.pending_expansion = Some(static_expanded);
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
                // Magic prediction: ALL commands extending the buffer become hints (the user
                // may not know the commands exist), with the raw buffer as the LAST rollback.
                // Space on a hint completes into that command; Space on the rollback commits
                // the raw `#xxx`.
                self.magic_hints = env.magic()
                    .hints(&self.buffer)
                    .into_iter()
                    .map(|(t, tok)| (t, tok.map(|s| s.to_string())))
                    .collect();
                let mut cands: Vec<String> = self.magic_hints.iter().map(|(t, _)| t.clone()).collect();
                cands.push(self.buffer.clone()); // rollback — last default option
                self.candidates = cands;
                self.candidates_fresh = true;
                self.candidate_highlight = 0;
                self.full_comp_count = self.candidates.len();
                self.partial_commit_indices = vec![false; self.candidates.len()];
                self.make_view()
            }
            Match::None => {
                self.buffer.push(ch);
                self.preedit = self.buffer.clone();
                self.cursor = self.preedit.len();
                self.magic_hints.clear();
                if self.buffer.starts_with('#') || self.buffer.starts_with('/') {
                    // Unknown trigger: DON'T vanish — keep the raw text as a
                    // fallback candidate (`/unknown` → candidate `/unknown`),
                    // Space commits it, Esc/Backspace cancel. The same trie
                    // fallback serves `#` and `/` uniformly.
                    self.candidates = vec![self.buffer.clone()];
                    self.candidates_fresh = true;
                    self.candidate_highlight = 0;
                    self.full_comp_count = 1;
                    self.partial_commit_indices = vec![false];
                    return self.make_view();
                }
                let text = std::mem::take(&mut self.buffer);
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
        let ranked = env.scorer().rank_detailed(&self.buffer, &self.context);
        self.last_meta = ranked.iter()
            .map(|c| (c.score, c.family, c.source))
            .collect();
        let cands: Vec<String> = ranked.into_iter().map(|c| c.text).collect();

        let mut merged = Vec::new();
        self.partial_commit_indices.clear();

        // Layer 3: if buffer has 2+ syllables, add first-syllable single-char
        // options for incremental composition (造词).
        // Interleave: a few top full comps, then single-char options.
        if let Some(first_syl) = env.first_syllable(&self.buffer) {
            if first_syl.len() < self.buffer.len() {
                let max_full = 8usize.min(cands.len());
                let max_chars = (CANDIDATE_SLOTS - max_full).min(8);
                let char_cands: Vec<String> = env.pinyin().candidates(&first_syl)
                    .into_iter()
                    .filter(|c| c.chars().count() == 1 && !merged.contains(c))
                    .take(max_chars)
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
                        .skip(max_full).filter(|&c| !merged.contains(c)).cloned()
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

    /// Called after a composed (自生词) multi-step selection completes — the
    /// result joins the phrase book unconditionally.
    fn learn_composed_phrase(&self, pinyin: &str, hanzi: &str);

    /// The magic command registry — spawns live member instances on trigger
    /// completion, holds the shared resources (voice slot, req config).
    fn magic(&self) -> &crate::family::magic::MagicFamily;
}
