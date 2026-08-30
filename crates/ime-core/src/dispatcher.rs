//! Dispatcher — holds the engine pieces and implements [`StepEnv`] for the FSM.
//! The stateful composition logic lives in [`state::StateMachine`].
//!
//! Candidate generation is delegated to the [`UnifiedScorer`], which collects
//! and ranks candidates from all enabled prediction families.

use crate::family::magic::expander::Expander;
use crate::family::english::EnglishFamily;
use crate::family::magic::MagicFamily;
use crate::family::pinyin::PinyinFamily;
use crate::family::CandidateFamily;
use crate::family::UnifiedScorer;
use crate::matcher::Matcher;
use crate::frontend::ImeView;
use crate::fsm::state::{StateMachine, StepEnv};
use std::sync::Arc;

pub struct Dispatcher {
    matcher: Matcher,
    expander: Expander,
    scorer: UnifiedScorer,
    /// 具体家族句柄(D5 接口隔离):学习/暖启/上下文开关等家族私有方法
    /// 走这里直调,不再经 trait 对象的 no-op 默认实现。scorer 持同一 Arc
    /// 当 trait 对象参与统一排序(见 family/mod.rs 的 Arc 委托 impl)。
    pinyin_family: Arc<PinyinFamily>,
    english_family: Arc<EnglishFamily>,
    /// The magic command registry — same `Arc` the engine holds, so late resource
    /// attachment (voice buffer, req base) is visible to the FSM and the members.
    magic: Arc<MagicFamily>,
}

impl Dispatcher {
    /// Full constructor. `magic` is the shared registry — the engine keeps the
    /// same `Arc` for resource attachment. `scoring` carries the configurable
    /// priorities/boosts/freq-scale from `swift-ime.yaml` (defaults = the legacy
    /// hardcoded values); `scoring.priorities` is the single source for every
    /// family's inter-family priority.
    pub fn with_config(
        matcher: Matcher,
        expander: Expander,
        magic: Arc<MagicFamily>,
        pinyin_weights: crate::family::pinyin::PinyinWeights,
        english_weights: crate::family::english::EnglishWeights,
        scoring: crate::scoring::ScoringConfig,
    ) -> Self {
        // pinyin + english + emoji compete in the unified scorer (中英混输 +
        // emoji). Magic (#) and snippet (/) are routed by the FSM via the
        // matcher — their candidates never pass through the scorer.
        let pinyin_family = Arc::new(PinyinFamily::with_scoring(pinyin_weights, scoring));
        let english_family = Arc::new(
            EnglishFamily::with_default_dict()
                .with_config(scoring.priorities.english, english_weights),
        );
        let emoji_family = crate::family::emoji::EmojiFamily::new();

        let scorer = UnifiedScorer::new(
            vec![
                Box::new(Arc::clone(&pinyin_family)),
                Box::new(Arc::clone(&english_family)),
                Box::new(emoji_family),
            ],
            scoring.priorities,
        );

        Dispatcher {
            matcher,
            expander,
            scorer,
            pinyin_family,
            english_family,
            magic,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(matcher: Matcher, expander: Expander) -> Self {
        // Minimal scorer for tests — just the pinyin family.
        let pinyin_family = Arc::new(PinyinFamily::new());
        let scorer = UnifiedScorer::new(
            vec![Box::new(Arc::clone(&pinyin_family))],
            crate::scoring::FamilyPriorities::default(),
        );

        Dispatcher {
            matcher,
            expander,
            scorer,
            pinyin_family,
            english_family: Arc::new(EnglishFamily::new()),
            magic: Arc::new(MagicFamily::new()),
        }
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

    /// Record a committed word for recency boosting —— 按提交家族分派(E2):
    /// 英文候选进 english 家族的 recency 表,其余(拼音)保持原路径。
    pub fn record_commit(&self, word: &str, family: Option<&str>) {
        if family == Some("english") {
            self.english_family.record_commit(word);
        } else {
            self.pinyin_family.record_commit(word);
        }
    }

    /// Warm the pinyin family's recent-member table from persisted data
    /// (`(word, last_used_ms)` pairs).
    pub fn warm_recencies(&self, entries: Vec<(String, i64)>) {
        self.pinyin_family.warm_recencies(entries);
    }

    /// Restore the inputx-pinyin L0 user model from persisted JSON.
    /// Returns the number of pins restored (0 if empty/invalid).
    pub fn import_l0(&self, json: &str) -> usize {
        self.pinyin_family.import_l0_json(json)
    }

    /// Attach weight store to families for persistence (pinyin phrases +
    /// english learned words).
    pub fn set_store(&self, store: Arc<crate::store::WeightStore>) {
        self.pinyin_family.attach_store(Arc::clone(&store));
        self.english_family.attach_store(store);
    }

    /// 学习英文自生词(Enter 强选 raw 文本提交时)。
    pub fn record_english_word(&self, word: &str) {
        self.english_family.record_learned_word(word);
    }

    /// Warm the english user layer from persisted 英文自生词。
    pub fn warm_en_user(&self, words: Vec<(String, u32)>) {
        self.english_family.warm_learned_words(&words);
    }

    /// 运行时启/禁某家族(`dicts.emoji: false` → "emoji" 全家族禁用)。
    pub fn set_family_enabled(&self, name: &str, on: bool) {
        if let Some(fam) = self.scorer.family(name) {
            fam.set_family_enabled(on);
        }
    }

    /// 临时关闭/恢复 pinyin 家族的上下文感知(swift-ime.yaml → input.context_aware)。
    pub fn set_pinyin_context_aware(&self, on: bool) {
        self.pinyin_family.set_context_aware(on);
    }

    /// E2:英文家族的上下文感知开关(recency boost)。
    pub fn set_english_context_aware(&self, on: bool) {
        self.english_family.set_context_aware(on);
    }

    /// Warm the phrase book from persisted SQLite data.
    pub fn warm_phrases_from_store(&self) {
        self.pinyin_family.warm_phrases_from_store();
    }

    /// Load an English user dictionary (all words get max priority).
    pub fn load_en_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer
            .family("english")
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "english family not found")
            })
            .and_then(|f| f.load_user_dict(path))
    }

    /// Load an external English dictionary (auto-detect type, normalize).
    pub fn load_en_dict(&self, path: &str) -> std::io::Result<usize> {
        self.scorer
            .family("english")
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "english family not found")
            })
            .and_then(|f| f.load_dict(path))
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
    fn matcher(&self) -> &Matcher {
        &self.matcher
    }
    fn expander(&self) -> &Expander {
        &self.expander
    }
    fn scorer(&self) -> &UnifiedScorer {
        &self.scorer
    }
    fn record_pick(&self, pinyin: &str, word: &str) {
        // 家族私有方法(D5):经具体句柄直调 —— 学习语义只有 pinyin 有。
        self.pinyin_family.record_pick(pinyin, word);
    }
    fn learn_phrase(&self, pinyin: &str, hanzi: &str) {
        self.pinyin_family.learn_phrase(pinyin, hanzi);
    }
    fn learn_composed_phrase(&self, pinyin: &str, hanzi: &str) {
        self.pinyin_family.learn_composed_phrase(pinyin, hanzi);
    }
    fn magic(&self) -> &MagicFamily {
        &self.magic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::family::magic::expander::StaticProvider;

    fn d() -> Dispatcher {
        let entries = vec![("#date".into(), "2026-07-23".into())];
        let d = Dispatcher::new_for_test(
            Matcher::new(entries),
            Expander::new(std::sync::Arc::new(StaticProvider {
                date: "2026-07-23".into(),
                clipboard: String::new(),
            })),
        );
        // 片段经 magic 注册表(`#/greet`),而非 matcher trie。
        d.magic()
            .set_snippets(vec![crate::store::snippet_md::SnippetEntry {
                name: "greet".into(),
                comment: String::new(),
                params: Vec::new(),
                template: "你好,我是 AI 秘书".into(),
            }]);
        d
    }

    fn sm() -> StateMachine {
        StateMachine::new()
    }

    #[test]
    fn idle_letter_enters_pinyin() {
        let d = d();
        let mut s = sm();
        let _v = d.process_key('n', &mut s);
        assert_eq!(
            s.state,
            crate::fsm::state::ComposeState::Pinyin,
            "single letter should enter pinyin state"
        );
        // 'n' alone is not a complete syllable; candidates depend on FST/decomp.
        // Subsequent typing of 'i' should produce candidates.
        let v = d.process_key('i', &mut s);
        assert!(v.candidate_count > 0, "ni should produce candidates");
    }

    #[test]
    fn snippet_expansion() {
        let d = d();
        let mut s = sm();
        // Type #/greet — shows expansion as candidate, doesn't auto-expand.
        let mut view = ImeView::empty();
        for c in "#/greet".chars() {
            view = d.process_key(c, &mut s);
        }
        assert!(
            view.candidate_count > 0,
            "should show expansion as candidate, got {view:?}"
        );
        // Space commits the expansion.
        assert_eq!(
            ImeView::str_field(&d.process_key(' ', &mut s).commit_text),
            "你好,我是 AI 秘书"
        );
    }

    #[test]
    fn pinyin_space_commits_top() {
        let d = d();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert_eq!(
            ImeView::str_field(&d.process_key(' ', &mut s).commit_text),
            "你"
        );
    }

    #[test]
    fn pinyin_enter_commits_raw() {
        let d = d();
        let mut s = sm();
        d.process_key('h', &mut s);
        d.process_key('e', &mut s);
        d.process_key('l', &mut s);
        d.process_key('l', &mut s);
        d.process_key('o', &mut s);
        assert_eq!(
            ImeView::str_field(&d.process_key('\n', &mut s).commit_text),
            "hello"
        );
    }

    #[test]
    fn pinyin_and_snippet_coexist() {
        let d = d();
        let mut s = sm();
        // Type #date — 静态命令预测为今天日期,space commits。
        d.process_key('#', &mut s);
        d.process_key('d', &mut s);
        d.process_key('a', &mut s);
        d.process_key('t', &mut s);
        d.process_key('e', &mut s);
        assert_eq!(
            ImeView::str_field(&d.process_key(' ', &mut s).commit_text),
            crate::family::magic::expander::today_str(),
            "#date commits today"
        );
        // After magic, typing letters enters pinyin.
        d.process_key('n', &mut s);
        let a = d.process_key('i', &mut s);
        assert!(
            a.candidate_count > 0,
            "after magic, ni should produce candidates, got {a:?}"
        );
    }

    #[test]
    fn select_candidate_commits_nth() {
        let d = d();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.process_key('i', &mut s);
        assert_eq!(
            ImeView::str_field(&d.select_candidate(1, &mut s).commit_text),
            "呢"
        );
    }

    #[test]
    fn snippet_cursor_places_caret_in_expanded_text() {
        // Template with a mid-text $CURSOR marker: committing places the caret
        // at the marker's offset in the EXPANDED text (variables before it are
        // variable-length, so the offset is computed after expansion).
        use crate::family::magic::expander::{Expander, VariableProvider};
        use std::sync::Mutex;

        #[derive(Default)]
        struct MutableDate {
            date: Mutex<String>,
        }
        impl VariableProvider for MutableDate {
            fn resolve(&self, name: &str) -> Option<String> {
                match name {
                    "DATE" => Some(self.date.lock().unwrap().clone()),
                    _ => None,
                }
            }
        }

        let provider: std::sync::Arc<dyn VariableProvider> = std::sync::Arc::new(MutableDate {
            date: Mutex::new("2026-08-05".into()),
        });
        let d = Dispatcher::new_for_test(
            Matcher::new(Vec::new()),
            Expander::new(provider),
        );
        d.magic()
            .set_snippets(vec![crate::store::snippet_md::SnippetEntry {
                name: "note".into(),
                comment: String::new(),
                params: Vec::new(),
                template: "$DATE 完成: $CURSOR 记得检查".into(),
            }]);
        let mut s = sm();
        for c in "#/note".chars() {
            d.process_key(c, &mut s);
        }
        let v = d.process_key(' ', &mut s);
        let text = ImeView::str_field(&v.commit_text);
        // "$DATE" = 10 bytes + " 完成: " = 9 → marker lands at byte 19.
        assert_eq!(
            text, "2026-08-05 完成:  记得检查",
            "marker removed from text"
        );
        assert_eq!(v.commit_cursor, 19, "caret mid-text, after the date prefix");
    }

    #[test]
    fn reset_clears_all() {
        let d = d();
        let mut s = sm();
        d.process_key('n', &mut s);
        d.reset(&mut s);
        assert!(s.buffer.is_empty());
        assert_eq!(s.state, crate::fsm::state::ComposeState::Idle);
    }
}
