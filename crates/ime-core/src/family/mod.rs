//! Candidate family system — pluggable prediction sources with unified
//! weight-based ranking.
//!
//! ## Architecture
//!
//! Each [`CandidateFamily`] independently generates scored candidates from
//! user input. The [`UnifiedScorer`] collects candidates from all enabled
//! families, applies inter-family priority weighting, deduplicates, and
//! returns a globally ranked list.
//!
//! ```text
//! Input → PinyinFamily (priority=100) → [候选A:0.95, 候选B:0.80, ...]
//!       → JianpinFamily (priority=85)  → [候选C:0.72, ...]
//!       → EnglishFamily (priority=60)  → [black:0.88, ...]
//!       → MagicFamily  (priority=95)   → [#date:1.0, ...]
//!       → SnippetFamily (priority=75)  → [/greet:1.0, ...]
//!                    ↓
//!       UnifiedScorer::rank()
//!         → final_score = raw_score × (priority / 100)
//!         → sort desc, dedup
//!                    ↓
//!            [最终排序列表]
//! ```

use std::collections::HashSet;

// ── ScoredCandidate ─────────────────────────────────────────────────────

/// A candidate word with its family origin and internal score.
#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    /// The candidate text (hanzi, English word, snippet expansion, etc.).
    pub text: String,
    /// Which family produced this candidate.
    pub family: &'static str,
    /// Which member within the family produced this candidate
    /// (e.g., "dict", "viterbi", "session", "phrase", "jianpin", "prefix").
    pub source: &'static str,
    /// Family-internal score in [0.0, 1.0]. Higher = better match.
    pub raw_score: f64,
}

/// A final-ranked candidate after global scoring and dedup.
/// Returned by [`UnifiedScorer::rank_detailed`].
#[derive(Debug, Clone)]
pub struct RankedCandidate {
    pub text: String,
    /// Final score after family priority weighting.
    pub score: f64,
    /// Which family ("pinyin", "english", ...).
    pub family: &'static str,
    /// Which member within the family ("dict", "viterbi", "session", ...).
    pub source: &'static str,
}

// ── InputContext ────────────────────────────────────────────────────────

/// Short-term input context — what the user recently committed.
/// Passed to every family's `predict` so they can adjust rankings
/// based on what came before.
#[derive(Debug, Clone, Default)]
pub struct InputContext {
    /// Last few committed characters (up to ~20 chars).
    pub recent_text: String,
    /// Last committed word (single word boundary).
    pub last_word: String,
}

impl InputContext {
    pub fn new() -> Self { InputContext::default() }

    pub fn update(&mut self, text: &str) {
        self.last_word = text.to_string();
        self.recent_text.push_str(text);
        // Keep only the last 20 characters.
        if self.recent_text.chars().count() > 20 {
            let skip = self.recent_text.chars().count() - 20;
            self.recent_text = self.recent_text.chars().skip(skip).collect();
        }
    }
}

// ── CandidateFamily trait ───────────────────────────────────────────────

/// A pluggable prediction source. Each family is an independent engine
/// that generates candidates from the raw input buffer.
pub trait CandidateFamily: Send + Sync {
    /// Unique family identifier (e.g., "pinyin", "jianpin").
    fn name(&self) -> &'static str;

    /// Inter-family priority (0–100). Higher-priority families get a
    /// larger multiplier in the final ranking.
    fn priority(&self) -> u32;

    /// Whether this family is currently active. Disabled families are
    /// skipped entirely by the [`UnifiedScorer`].
    fn enabled(&self) -> bool {
        true
    }

    /// How many top candidates this family sends to the inter-family
    /// competition. Default: 8.
    fn top_n(&self) -> usize {
        8
    }

    /// Generate scored candidates from the raw input buffer.
    fn predict(&self, input: &str) -> Vec<ScoredCandidate>;

    /// Generate context-aware candidates. Default implementation delegates
    /// to [`predict`]. Families that support context (e.g. PinyinFamily
    /// boosting based on the previous character, AIFamily generating full
    /// sentences) override this method.
    fn predict_with_context(&self, input: &str, _ctx: &InputContext) -> Vec<ScoredCandidate> {
        self.predict(input)
    }

    /// Load an external dictionary file into this family's vocabulary.
    fn load_dict(&self, _path: &str) -> std::io::Result<usize> {
        Ok(0)
    }

    /// Load dictionary entries from raw TSV bytes (for embedded dicts).
    fn load_dict_bytes(&self, _data: &[u8]) -> usize {
        0
    }

    /// Record a user pick for frequency boosting (per-family auto-learning).
    fn record_pick(&self, _pinyin: &str, _word: &str) {}

    /// Learn a new phrase for future recall.
    fn learn_phrase(&self, _pinyin: &str, _word: &str) {}

    /// Export L0 user model as JSON (pins + pick counters).
    fn export_l0_json(&self) -> String { String::new() }

    /// Import L0 user model from JSON. Returns pins restored.
    fn import_l0_json(&self, _json: &str) -> usize { 0 }
}

// ── UnifiedScorer ───────────────────────────────────────────────────────

/// Collects candidates from all enabled families and returns a globally
/// ranked, deduplicated list.
pub struct UnifiedScorer {
    families: Vec<Box<dyn CandidateFamily>>,
}

impl UnifiedScorer {
    pub fn new(families: Vec<Box<dyn CandidateFamily>>) -> Self {
        UnifiedScorer { families }
    }

    /// Build with the standard five families.
    #[cfg(test)]
    pub fn with_defaults(
        pinyin: Box<dyn CandidateFamily>,
        snippet: Box<dyn CandidateFamily>,
        magic: Box<dyn CandidateFamily>,
        english: Box<dyn CandidateFamily>,
        jianpin: Box<dyn CandidateFamily>,
    ) -> Self {
        UnifiedScorer {
            families: vec![pinyin, jianpin, magic, snippet, english],
        }
    }

    /// Rank all candidates (context-free). Returns deduplicated texts.
    pub fn rank(&self, input: &str) -> Vec<String> {
        self.rank_with_context(input, &InputContext::new())
    }

    /// Rank candidates with context. Returns deduplicated texts.
    pub fn rank_with_context(&self, input: &str, ctx: &InputContext) -> Vec<String> {
        self.rank_detailed(input, ctx).into_iter().map(|d| d.text).collect()
    }

    /// Rank with full detail — each result includes source and score.
    /// This is the core ranking algorithm; `rank` / `rank_with_context`
    /// delegate here and strip the metadata.
    pub fn rank_detailed(&self, input: &str, ctx: &InputContext) -> Vec<RankedCandidate> {
        if input.is_empty() { return Vec::new(); }

        let mut scored: Vec<(f64, RankedCandidate)> = Vec::new();

        for family in &self.families {
            if !family.enabled() { continue; }
            let priority_bonus = family.priority() as f64 / 100.0;
            let mut candidates = family.predict_with_context(input, ctx);

            candidates.sort_by(|a, b| {
                b.raw_score.partial_cmp(&a.raw_score).unwrap_or(std::cmp::Ordering::Equal)
            });

            for c in candidates.into_iter().take(family.top_n()) {
                let final_score = c.raw_score * priority_bonus;
                scored.push((final_score, RankedCandidate {
                    text: c.text,
                    score: final_score,
                    family: c.family,
                    source: c.source,
                }));
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut seen = HashSet::new();
        scored.into_iter()
            .filter(|(_, rc)| seen.insert(rc.text.clone()))
            .map(|(_, rc)| rc)
            .collect()
    }

    /// Number of registered families (including disabled ones).
    pub fn family_count(&self) -> usize {
        self.families.len()
    }

    /// Access a family by name.
    pub fn family(&self, name: &str) -> Option<&dyn CandidateFamily> {
        self.families.iter().find(|f| f.name() == name).map(|f| &**f)
    }

    /// Load an external dictionary into the named family.
    pub fn load_dict_to(&self, family_name: &str, path: &str) -> Option<std::io::Result<usize>> {
        self.family(family_name).map(|f| f.load_dict(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubFamily {
        name: &'static str,
        priority: u32,
        candidates: Vec<(&'static str, f64)>,
    }

    impl CandidateFamily for StubFamily {
        fn name(&self) -> &'static str { self.name }
        fn priority(&self) -> u32 { self.priority }
        fn predict(&self, _input: &str) -> Vec<ScoredCandidate> {
            self.candidates.iter().map(|(t, s)| ScoredCandidate {
                text: t.to_string(), family: self.name, source: "stub", raw_score: *s,
            }).collect()
        }
    }

    #[test]
    fn higher_priority_wins_tie() {
        let fam_a = StubFamily { name: "A", priority: 100, candidates: vec![("word", 0.5)] };
        let fam_b = StubFamily { name: "B", priority: 50, candidates: vec![("word", 0.5)] };
        let scorer = UnifiedScorer::new(vec![Box::new(fam_a), Box::new(fam_b)]);
        let result = scorer.rank("test");
        assert_eq!(result.len(), 1); // deduped to one
        assert_eq!(result[0], "word");
    }

    #[test]
    fn higher_score_within_family_wins() {
        let fam = StubFamily { name: "P", priority: 100, candidates: vec![
            ("low", 0.3), ("high", 0.9), ("mid", 0.6),
        ]};
        let scorer = UnifiedScorer::new(vec![Box::new(fam)]);
        let result = scorer.rank("test");
        assert_eq!(&result[..3], &["high", "mid", "low"]);
    }

    #[test]
    fn cross_family_ranking() {
        let pinyin = StubFamily { name: "pinyin", priority: 100, candidates: vec![
            ("候选A", 0.7),
        ]};
        let english = StubFamily { name: "english", priority: 60, candidates: vec![
            ("black", 1.0),  // raw 1.0 × 0.60 = 0.60 final
        ]};
        let scorer = UnifiedScorer::new(vec![Box::new(pinyin), Box::new(english)]);
        let result = scorer.rank("bla");
        // 候选A: 0.7 × 1.0 = 0.70, black: 1.0 × 0.6 = 0.60
        assert_eq!(result[0], "候选A");
        assert_eq!(result[1], "black");
    }

    #[test]
    fn disabled_family_skipped() {
        struct DisabledFamily;
        impl CandidateFamily for DisabledFamily {
            fn name(&self) -> &'static str { "disabled" }
            fn priority(&self) -> u32 { 100 }
            fn enabled(&self) -> bool { false }
            fn predict(&self, _: &str) -> Vec<ScoredCandidate> {
                vec![ScoredCandidate { text: "nope".into(), family: "disabled", source: "stub", raw_score: 1.0 }]
            }
        }
        let fam = StubFamily { name: "ok", priority: 50, candidates: vec![("yes", 0.5)] };
        let scorer = UnifiedScorer::new(vec![Box::new(DisabledFamily), Box::new(fam)]);
        let result = scorer.rank("x");
        assert_eq!(result, vec!["yes"]);
    }

    #[test]
    fn empty_input_returns_empty() {
        let fam = StubFamily { name: "P", priority: 100, candidates: vec![("x", 0.5)] };
        let scorer = UnifiedScorer::new(vec![Box::new(fam)]);
        assert!(scorer.rank("").is_empty());
    }
}

// ── Submodule declarations ──────────────────────────────────────────────

pub mod ai;
pub mod emoji;
pub mod english;
pub mod magic;
pub mod pinyin;
pub mod snippet;

