//! Dispatcher — holds the engine pieces and implements [`StepEnv`] for the FSM.
//! The stateful composition logic lives in [`state::StateMachine`].
//!
//! Candidate generation is delegated to the [`UnifiedScorer`], which collects
//! and ranks candidates from all enabled prediction families.

use crate::expander::Expander;
use crate::family::english::EnglishFamily;
use crate::family::magic::MagicFamily;
use crate::family::pinyin::PinyinFamily;
use crate::family::snippet::SnippetFamily;
use crate::family::{CandidateFamily, UnifiedScorer};
use crate::matcher::Matcher;
use crate::family::pinyin::engine::InputxPinyin;
use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};
use crate::PinyinEngine;
use std::sync::Arc;

pub struct Dispatcher {
    matcher: Matcher,
    expander: Expander,
    pinyin: Box<dyn PinyinEngine>,
    scorer: UnifiedScorer,
    /// The magic command registry — same `Arc` the engine holds, so late resource
    /// attachment (voice buffer, req base) is visible to the FSM and the members.
    magic: Arc<MagicFamily>,
}

impl Dispatcher {
    /// Full constructor with configurable English family settings. `magic` is the
    /// shared registry — the engine keeps the same `Arc` for resource attachment.
    pub fn with_config(
        matcher: Matcher,
        expander: Expander,
        magic: Arc<MagicFamily>,
        pinyin_weights: crate::family::pinyin::PinyinWeights,
        english_priority: u32,
        english_weights: crate::family::english::EnglishWeights,
    ) -> Self {
        let pinyin_family = PinyinFamily::with_weights(pinyin_weights);
        let snippet_family = SnippetFamily::new(matcher.clone(), expander.clone());
        let magic_family: Box<dyn CandidateFamily> = Box::new((*magic).clone());
        let english_family = EnglishFamily::with_default_dict()
            .with_config(english_priority, english_weights);

        // Build in priority order.
        let scorer = UnifiedScorer::new(vec![
            Box::new(pinyin_family),
            magic_family,
            Box::new(snippet_family),
            Box::new(english_family),
        ]);

        Dispatcher {
            matcher,
            expander,
            pinyin: Box::new(InputxPinyin::new()),
            scorer,
            magic,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(
        matcher: Matcher,
        expander: Expander,
        pinyin: Box<dyn PinyinEngine>,
    ) -> Self {
        // Minimal scorer for tests — just the pinyin family.
        let pinyin_only = PinyinFamily::new();
        let scorer = UnifiedScorer::new(vec![Box::new(pinyin_only)]);

        Dispatcher { matcher, expander, pinyin, scorer, magic: Arc::new(MagicFamily::new()) }
    }

    pub fn process_key(&self, ch: char, sm: &mut StateMachine) -> ImeView {
        sm.step(ch, self)
    }

    pub fn select_candidate(&self, index: usize, sm: &mut StateMachine) -> ImeView {
        sm.select(index, self)
    }

    pub fn reset(&self, sm: &mut StateMachine) {
        sm.reset();
    }

    /// Record a bigram to the in-memory pinyin family model.
    pub fn record_bigram(&self, prev: &str, next: &str) {
        if let Some(fam) = self.scorer.family("pinyin") {
            fam.record_bigram(prev, next);
        }
    }

    /// Record a committed word for recency boosting.
    pub fn record_commit(&self, word: &str) {
        if let Some(fam) = self.scorer.family("pinyin") {
            fam.record_commit(word);
        }
    }

    /// Warm the pinyin family's in-memory bigram model from persisted SQLite data.
    pub fn warm_bigrams(&self, entries: Vec<(String, String, u32)>) {
        if let Some(fam) = self.scorer.family("pinyin") {
            fam.warm_bigrams(entries);
        }
    }

    /// Attach weight store to pinyin family for phrase persistence.
    pub fn set_store(&self, store: Arc<crate::weight_store::WeightStore>) {
        if let Some(fam) = self.scorer.family("pinyin") {
            fam.attach_store(store);
        }
    }

    /// Warm the phrase book from persisted SQLite data.
    pub fn warm_phrases_from_store(&self) {
        if let Some(fam) = self.scorer.family("pinyin") {
            fam.warm_phrases_from_store();
        }
    }

    /// Load an English user dictionary (all words get max priority).
    pub fn load_en_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer.family("english")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "english family not found"))
            .and_then(|f| f.load_user_dict(path))
    }

    /// Load an external English dictionary (auto-detect type, normalize).
    pub fn load_en_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer.family("english")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "english family not found"))
            .and_then(|f| f.load_dict(path))
    }

    /// Attach the voice buffer to the magic registry's shared slot — read by the
    /// `#asr`/`#submit` member instances.
    pub fn set_asr_buffer(&self, buf: std::sync::Arc<crate::asr_buffer::AsrBuffer>) {
        self.magic.set_asr_buffer(buf);
    }

    /// The magic command registry.
    pub fn magic(&self) -> &MagicFamily {
        &self.magic
    }

    pub fn reload_matcher(&mut self, entries: Vec<(String, String)>) {
        self.matcher = Matcher::new(entries);
    }
}

impl StepEnv for Dispatcher {
    fn matcher(&self) -> &Matcher { &self.matcher }
    fn expander(&self) -> &Expander { &self.expander }
    fn pinyin(&self) -> &dyn PinyinEngine { &*self.pinyin }
    fn scorer(&self) -> &UnifiedScorer { &self.scorer }
    fn first_syllable(&self, pinyin: &str) -> Option<String> {
        self.pinyin.first_syllable(pinyin)
    }
    fn record_pick(&self, pinyin: &str, word: &str) {
        // Route through PinyinFamily (via scorer) for per-family auto-learning.
        if let Some(fam) = self.scorer.family("pinyin") {
            fam.record_pick(pinyin, word);
        }
    }
    fn learn_phrase(&self, pinyin: &str, hanzi: &str) {
        if let Some(fam) = self.scorer.family("pinyin") {
            fam.learn_phrase(pinyin, hanzi);
        }
    }
    fn magic(&self) -> &MagicFamily {
        &self.magic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expander::StaticProvider;

    struct StubPinyin;
    impl PinyinEngine for StubPinyin {
        fn first_syllable(&self, pinyin: &str) -> Option<String> {
            let cand = &pinyin[..pinyin.len().min(2)];
            if cand.chars().all(|c| c.is_ascii_lowercase()) {
                Some(cand.to_string())
            } else {
                None
            }
        }
        fn record_pick(&self, _pinyin: &str, _word: &str) {}
        fn learn_phrase(&self, _pinyin: &str, _hanzi: &str) {}
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
        let _v = d.process_key('n', &mut s);
        assert_eq!(s.state, crate::state::ComposeState::Pinyin,
            "single letter should enter pinyin state");
        // 'n' alone is not a complete syllable; candidates depend on FST/decomp.
        // Subsequent typing of 'i' should produce candidates.
        let v = d.process_key('i', &mut s);
        assert!(v.candidate_count > 0, "ni should produce candidates");
    }

    #[test]
    fn snippet_expansion() {
        let d = d(); let mut s = sm();
        // Type /greet — shows expansion as candidate, doesn't auto-expand.
        d.process_key('/', &mut s); d.process_key('g', &mut s); d.process_key('r', &mut s); d.process_key('e', &mut s); d.process_key('e', &mut s);
        let view = d.process_key('t', &mut s);
        assert!(view.candidate_count > 0, "should show expansion as candidate, got {view:?}");
        // Space commits the expansion.
        assert_eq!(ImeView::str_field(&d.process_key(' ', &mut s).commit_text), "你好,我是 AI 秘书");
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
        // Type #date — shows candidate, space commits.
        d.process_key('#', &mut s); d.process_key('d', &mut s); d.process_key('a', &mut s); d.process_key('t', &mut s);
        d.process_key('e', &mut s);
        assert_eq!(ImeView::str_field(&d.process_key(' ', &mut s).commit_text), "2026-07-23");
        // After snippet, typing letters enters pinyin.
        d.process_key('n', &mut s);
        let a = d.process_key('i', &mut s);
        assert!(a.candidate_count > 0, "after snippet, ni should produce candidates, got {a:?}");
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
