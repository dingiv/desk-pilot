//! PinyinFamily — Chinese full-pinyin prediction using inputx-pinyin's
//! embedded dictionary + bigram Viterbi composition + PhraseBook recall.

use super::{CandidateFamily, InputContext, ScoredCandidate};
use self::phrase::PhraseBook;
use crate::recency::RecencyStore;
use crate::user_bigram::UserBigram;

pub mod dict;
pub mod engine;
pub mod lattice;
pub mod phrase;

use dict::LargeDict;
use std::sync::{Arc, Mutex};

use crate::weight_store::WeightStore;

/// Full-pinyin prediction family.
///
/// Scoring:
/// - LargeDict exact match: `raw_score = 1.0` (900K+ entries, O(1))
/// - PhraseBook exact match: `raw_score = 0.95` (small, user-custom)
/// - Viterbi composition: normalized log-likelihood → [0.3, 0.95]
/// - Session lookup: `raw_score = 0.5`
/// - Prefix fallback: `raw_score = 0.3`
/// - PhraseBook prefix: `raw_score = 0.85`
pub struct PinyinFamily {
    engine: inputx_pinyin::PinyinEngine,
    phrase_book: Mutex<PhraseBook>,
    large_dict: Mutex<LargeDict>,
    lattice: Mutex<Option<lattice::LatticeDecoder>>,
    bigram: Mutex<UserBigram>,
    recency: Mutex<RecencyStore>,
    enabled: bool,
    weights: PinyinWeights,
    store: Mutex<Option<Arc<WeightStore>>>,
    /// freq→score 映射参数(swift-ime.yaml → weights.freq_scale)。
    freq_scale: crate::scoring::FreqScale,
}

/// Configurable scoring weights for the pinyin family.
/// All values are tunable via swift-ime.yaml → weights.pinyin section.
#[derive(Debug, Clone)]
pub struct PinyinWeights {
    // ── Member base scores ──
    pub phrase_book: f64,
    pub large_dict: f64,
    pub viterbi_base: f64,
    pub viterbi_scale: f64,
    pub jianpin: f64,
    pub single_syl_decay: f64,
    pub context_boost: f64,
    // ── Post-merge adjustments ──
    pub stopword_penalty: f64,   // multiplier for all-stopword compositions
    pub confirm_bonus: f64,      // bonus for dict∩viterbi confirmation
    pub short_word_bonus: f64,   // bonus per 2-char word
    // ── Take limits ──
    pub large_dict_take: usize,
    pub viterbi_take: usize,
    pub jianpin_take: usize,
}

impl Default for PinyinWeights {
    fn default() -> Self {
        PinyinWeights {
            phrase_book: 0.88, large_dict: 0.85,
            viterbi_base: 0.25, viterbi_scale: 0.55,
            jianpin: 0.50,
            single_syl_decay: 0.5, context_boost: 0.12,
            stopword_penalty: 0.5, confirm_bonus: 0.05, short_word_bonus: 0.01,
            large_dict_take: 96, viterbi_take: 48,
            jianpin_take: 8,
        }
    }
}

impl PinyinFamily {
    pub fn new() -> Self {
        Self::with_scoring(PinyinWeights::default(), crate::scoring::ScoringConfig::default())
    }

    pub fn with_weights(weights: PinyinWeights) -> Self {
        Self::with_scoring(weights, crate::scoring::ScoringConfig::default())
    }

    /// Full construction: pinyin weights + the unified scoring config (recency
    /// boosts, bigram ceiling, freq→score scale) from `swift-ime.yaml`.
    pub fn with_scoring(
        weights: PinyinWeights,
        scoring: crate::scoring::ScoringConfig,
    ) -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(PhraseBook::default_phrases()),
            large_dict: Mutex::new(LargeDict::new()),
            lattice: Mutex::new(None),
            bigram: Mutex::new(UserBigram::with_tuning(scoring.bigram)),
            recency: Mutex::new(RecencyStore::new(scoring.recency)),
            enabled: true,
            weights,
            store: Mutex::new(None),
            freq_scale: scoring.freq_scale,
        }
    }

    pub fn set_weights(&mut self, w: PinyinWeights) { self.weights = w; }

    pub fn with_phrase_book(phrase_book: PhraseBook) -> Self {
        Self::with_scoring_and_phrase_book(
            PinyinWeights::default(),
            crate::scoring::ScoringConfig::default(),
            phrase_book,
        )
    }

    fn with_scoring_and_phrase_book(
        weights: PinyinWeights,
        scoring: crate::scoring::ScoringConfig,
        phrase_book: PhraseBook,
    ) -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(phrase_book),
            large_dict: Mutex::new(LargeDict::new()),
            lattice: Mutex::new(None),
            bigram: Mutex::new(UserBigram::with_tuning(scoring.bigram)),
            recency: Mutex::new(RecencyStore::new(scoring.recency)),
            enabled: true,
            weights,
            store: Mutex::new(None),
            freq_scale: scoring.freq_scale,
        }
    }

    /// Attach the weight store for persisting learned phrases.
    pub fn set_store(&self, store: Arc<WeightStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    /// Warm the phrase book from persisted SQLite data (internal helper).
    fn do_warm_phrases(&self) {
        let guard = self.store.lock().unwrap();
        if let Some(ref store) = *guard {
            let entries = store.load_all_phrases();
            if !entries.is_empty() {
                let mut book = self.phrase_book.lock().unwrap();
                for (pinyin, word, priority) in &entries {
                    book.insert_with_order(pinyin, word, *priority);
                }
                eprintln!("[ime-core] pinyin: warmed {} phrases from store", entries.len());
            }
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) { self.enabled = enabled; }

    pub fn record_bigram(&self, prev: &str, next: &str) {
        self.bigram.lock().unwrap().record(prev, next);
    }

    /// Record a committed word for recency boosting.
    /// Double-writes the ring to SQLite (full-snapshot replace, ≤64 rows) so the
    /// boost-decay order survives restarts.
    pub fn record_commit(&self, word: &str) {
        let mut rec = self.recency.lock().unwrap();
        rec.push(word);
        if let Some(ref store) = *self.store.lock().unwrap() {
            store.save_recency(&rec.dump());
        }
    }

    pub fn bigram_json(&self) -> String {
        self.bigram.lock().unwrap().export_json()
    }

    pub fn import_bigram_json(&self, json: &str) {
        self.bigram.lock().unwrap().import_json(json);
    }

    pub fn record_pick(&self, pinyin: &str, word: &str) {
        self.engine.dict().record_pick(pinyin, word);
    }

    pub fn engine(&self) -> &inputx_pinyin::PinyinEngine { &self.engine }
    pub fn phrase_count(&self) -> usize { self.phrase_book.lock().unwrap().len() }
    pub fn large_dict_len(&self) -> usize { self.large_dict.lock().unwrap().len() }
}

/// Extract initials from a raw (concatenated) pinyin string.
/// "shengnengshengqiao" → "snsq", "nihao" → "nh".
pub fn initials_from_pinyin(raw: &str) -> String {
    let segs = inputx_pinyin::segment(raw);
    segs.first()
        .map(|seg| seg.syllables.iter().filter_map(|s| s.chars().next()).collect())
        .unwrap_or_default()
}

impl Default for PinyinFamily {
    fn default() -> Self { Self::new() }
}

impl CandidateFamily for PinyinFamily {
    fn name(&self) -> &'static str { "pinyin" }
    fn priority(&self) -> u32 { 100 }
    fn enabled(&self) -> bool { self.enabled }
    fn top_n(&self) -> usize { 128 }

    fn record_pick(&self, pinyin: &str, word: &str) {
        self.engine.dict().record_pick(pinyin, word);
        self.learn_phrase(pinyin, word);
        // Persist the L0 user model (pins + pick counters) — same double-write
        // cadence as bigrams, so the 3-pick auto-pin survives restarts.
        if let Some(ref store) = *self.store.lock().unwrap() {
            store.save_l0(&self.export_l0_json());
        }
    }

    fn learn_phrase(&self, pinyin: &str, hanzi: &str) {
        let mut book = self.phrase_book.lock().unwrap();
        if !book.exact(pinyin).contains(&hanzi.to_string()) {
            book.insert(pinyin, hanzi);
            // Persist to SQLite if store is attached.
            if let Some(ref store) = *self.store.lock().unwrap() {
                store.record_phrase(pinyin, hanzi, 0);
            }
        }
    }

    fn export_l0_json(&self) -> String {
        let snap = self.engine.dict().export_l0();
        let mut json = String::from("{\"pins\":[");
        for (i, (py, w)) in snap.pins.iter().enumerate() {
            if i > 0 { json.push(','); }
            json.push_str(&format!("[\"{py}\",\"{w}\"]"));
        }
        json.push_str("],\"picks\":[");
        let mut first = true;
        for (py, w, c) in &snap.pick_counts {
            if !first { json.push(','); } first = false;
            json.push_str(&format!("[\"{py}\",\"{w}\",{c}]"));
        }
        json.push_str("]}");
        json
    }

    fn import_l0_json(&self, json: &str) -> usize {
        #[derive(serde::Deserialize)]
        struct L0Json { pins: Vec<(String, String)>, #[serde(default)] picks: Vec<(String, String, u32)> }
        if let Ok(data) = serde_json::from_str::<L0Json>(json) {
            let snap = inputx_pinyin::L0Snapshot { pins: data.pins, pick_counts: data.picks };
            self.engine.dict().import_l0(snap)
        } else { 0 }
    }

    fn record_bigram(&self, prev: &str, next: &str) {
        self.bigram.lock().unwrap().record(prev, next);
    }

    fn warm_bigrams(&self, entries: Vec<(String, String, u32)>) {
        if !entries.is_empty() {
            let count = entries.len();
            self.bigram.lock().unwrap().load_bulk(entries);
            eprintln!("[ime-core] pinyin: warmed {count} bigrams from store");
        }
    }

    fn warm_recencies(&self, entries: Vec<String>) {
        if !entries.is_empty() {
            let count = entries.len();
            // Persisted dump is most-recent-first; load_bulk pushes each entry
            // to the front, so feed OLDEST first to end with the newest on top.
            let mut oldest_first = entries;
            oldest_first.reverse();
            self.recency.lock().unwrap().load_bulk(&oldest_first);
            eprintln!("[ime-core] pinyin: warmed {count} recency entries from store");
        }
    }

    fn record_commit(&self, word: &str) {
        // Delegate to the inherent impl (which pushes + persists the ring).
        PinyinFamily::record_commit(self, word);
    }

    fn attach_store(&self, store: std::sync::Arc<WeightStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    fn warm_phrases_from_store(&self) {
        self.do_warm_phrases();
    }

    fn load_dict_bytes(&self, data: &[u8]) -> usize {
        if data.len() > 100_000 {
            let n = self.large_dict.lock().unwrap().load_from_tsv_bytes(data);
            // After loading, try to build lattice from the FST.
            // LargeDict's backend stores the FST; we can't access it directly.
            // For now, lattice is built when loading from file (load_fst_file).
            n
        } else {
            self.phrase_book.lock().unwrap().load_from_tsv_bytes(data)
        }
    }

    fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        if path.ends_with(".json") {
            let json = std::fs::read_to_string(path)?;
            let mut book = self.phrase_book.lock().unwrap();
            book.load_from_json_str(&json)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        } else if path.ends_with(".fst") {
            // FST: load and build LatticeDecoder, passing path for .idx cache.
            let data = std::fs::read(path)?;
            let dict = inputx_fsa::Dict::new(data)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}")))?;
            *self.lattice.lock().unwrap() = Some(lattice::LatticeDecoder::new(dict, path));
            Ok(0) // size not tracked for FST
        } else {
            let meta = std::fs::metadata(path)?;
            if meta.len() > 100_000 {
                self.large_dict.lock().unwrap().load_from_tsv_file(path)
            } else {
                self.phrase_book.lock().unwrap().load_from_tsv(path)
            }
        }
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() { return Vec::new(); }

        let dict = self.engine.dict();
        let mut out = Vec::new();

        let is_single_syllable = inputx_pinyin::is_valid_syllable(input);

        // ── Primary: single-syllable lookup or unified lattice ──
        if is_single_syllable {
            let words = dict.lookup(input);
            let total = words.len().max(1) as f64;
            for (i, word) in words.into_iter().enumerate() {
                out.push(ScoredCandidate {
                    text: word, family: "pinyin", source: "single",
                    raw_score: (self.weights.large_dict - (i as f64 / total) * self.weights.single_syl_decay).clamp(0.0, 1.0),
                });
            }
        } else {
            // Unified lattice: handles full pinyin, jianpin, mixed in one pass.
            // Full pinyin matches keep full freq_to_score; mixed/initials
            // (简拼/混写) are discounted by the jianpin weight so they
            // don't drown out English exact matches for ambiguous inputs.
            let lattice_guard = self.lattice.lock().unwrap();
            if let Some(ref lat) = *lattice_guard {
                let results = lat.predict(input, self.weights.large_dict_take);
                for r in results {
                    let base_score = lat.freq_to_score(&self.freq_scale, r.freq_score as u64);
                    let (source, score) = match r.match_type {
                        lattice::MatchType::Full => ("lattice", base_score),
                        lattice::MatchType::Mixed => ("lattice_mix", base_score * self.weights.jianpin),
                        lattice::MatchType::Initials => ("lattice_jp", base_score * self.weights.jianpin),
                    };
                    out.push(ScoredCandidate {
                        text: r.text, family: "pinyin", source,
                        raw_score: score,
                    });
                }
            }
            drop(lattice_guard);

            // Viterbi decomposition — always runs as fallback (造词).
            let comps = dict.top_k_compositions(input, self.weights.viterbi_take);
            for (_s, word) in comps.iter().take(16) {
                if !out.iter().any(|c| c.text == *word) {
                    out.push(ScoredCandidate {
                        text: word.clone(), family: "pinyin", source: "decomp",
                        raw_score: 0.4,
                    });
                }
            }
        }

        // ── PhraseBook: user phrases promote new words, never downgrade dict hits ──
        // A learned word that ALSO exists in the dictionary keeps its dict score
        // when that's higher (previously the phrase entry REPLACED it at the
        // fixed 0.88 — e.g. 继续's full-pinyin hit dropped below 急须). Only when
        // the dict hit scores LOWER (rare/low-frequency word the user favors)
        // does the phrase entry take over.
        {
            let book = self.phrase_book.lock().unwrap();
            for w in book.exact(input) {
                let dict_score = out.iter().find(|c| c.text == w).map(|c| c.raw_score);
                match dict_score {
                    Some(s) if s >= self.weights.phrase_book => {
                        // Dict hit already ≥ phrase score — keep it (do not
                        // retain-remove + re-add at the lower fixed score).
                    }
                    _ => {
                        out.retain(|c| c.text != w);
                        out.push(ScoredCandidate { text: w, family: "pinyin", source: "phrase", raw_score: self.weights.phrase_book });
                    }
                }
            }
            // ── PhraseBook initials match (lzm → 李正明) ──
            for w in book.by_initials(input) {
                if !out.iter().any(|c| c.text == w) {
                    out.push(ScoredCandidate {
                        text: w, family: "pinyin", source: "phrase_sp",
                        raw_score: self.weights.phrase_book * 0.95,
                    });
                }
            }
        }

        // ── Short-word bonus ──
        for c in &mut out {
            if c.text.chars().count() == 2 {
                c.raw_score = (c.raw_score + self.weights.short_word_bonus).min(1.0);
            }
        }

        if !out.is_empty() {
            out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
            return out;
        }

        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out
    }

    fn predict_with_context(&self, input: &str, ctx: &InputContext) -> Vec<ScoredCandidate> {
        let mut candidates = self.predict(input);
        if candidates.is_empty() { return candidates; }

        // ── Layer 1: Recency boost (short-term memory) ──
        let recency = self.recency.lock().unwrap();
        if !recency.is_empty() {
            for c in &mut candidates {
                c.raw_score = (c.raw_score + recency.boost(&c.text)).min(1.0);
            }
        }
        drop(recency);

        let dict = self.engine.dict();
        let bigram = self.bigram.lock().unwrap();
        // Build context word list: recent commits + last word + surrounding text.
        let surr_words: Vec<&str> = ctx.surrounding.split_whitespace().collect();
        let ctx_words: Vec<String> = ctx.recent_text.split_whitespace()
            .chain(std::iter::once(ctx.last_word.as_str()))
            .chain(surr_words.iter().copied())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_string())
            .collect();
        if !bigram.is_empty() {
            for c in &mut candidates {
                let boost = bigram.boost(&ctx_words, &c.text);
                c.raw_score = (c.raw_score * boost).min(1.0);
            }
        }

        // ── HistoryBigram-style word-level boosting ────
        // For each word in the recent context, boost candidates that form
        // known bigrams with that word. This mirrors libime's UserLanguageModel.
        for ctx_word in ctx.recent_text.split_whitespace().chain(
            std::iter::once(ctx.last_word.as_str())
        ) {
            if ctx_word.is_empty() || ctx_word.chars().count() < 1 { continue; }
            for c in &mut candidates {
                // Check if ctx_word + candidate[0] forms a known word.
                let first_c = c.text.chars().next().unwrap_or('\0');
                if first_c == '\0' { continue; }
                let combined: String = ctx_word.chars().chain(std::iter::once(first_c)).collect();
                // Quick check: look up the combined word in the dictionary.
                for py_ctx in inputx_pinyin::char_to_pinyin(ctx_word.chars().next().unwrap()) {
                    for py_cand in inputx_pinyin::char_to_pinyin(first_c) {
                        let combined_py = format!("{py_ctx}{py_cand}");
                        if dict.lookup(&combined_py).iter().any(|w| w.starts_with(&combined)) {
                            c.raw_score = (c.raw_score + self.weights.context_boost).min(1.0);
                        }
                    }
                }
            }
        }

        candidates.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinyin_family_nihao() {
        let fam = PinyinFamily::new();
        let cands = fam.predict("nihao");
        assert!(!cands.is_empty());
        assert!(cands.iter().any(|c| c.text.contains("你好")));
    }

    #[test]
    fn pinyin_family_xiayige() {
        let fam = PinyinFamily::new();
        let cands = fam.predict("xiayige");
        assert!(cands.iter().any(|c| c.text == "下一个"));
    }

    #[test]
    fn phrase_book_recall() {
        let fam = PinyinFamily::new();
        fam.learn_phrase("lisa", "丽萨");
        let cands = fam.predict("lisa");
        assert_eq!(&cands[0].text, "丽萨",
            "learned phrase should be top, got {:?}", cands.iter().map(|c| &c.text).take(5).collect::<Vec<_>>());
    }

    #[test]
    fn returns_scored_candidates() {
        let fam = PinyinFamily::new();
        let cands = fam.predict("nihao");
        for c in &cands {
            assert!(c.raw_score >= 0.0 && c.raw_score <= 1.0);
            assert_eq!(c.family, "pinyin");
        }
    }
}
