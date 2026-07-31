//! PinyinFamily — Chinese full-pinyin prediction using inputx-pinyin's
//! embedded dictionary + bigram Viterbi composition + PhraseBook recall.

use super::{CandidateFamily, InputContext, ScoredCandidate};
use self::phrase::PhraseBook;
use crate::user_bigram::UserBigram;

pub mod dict;
pub mod engine;
pub mod phrase;

use dict::LargeDict;
use std::sync::Mutex;

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
    bigram: Mutex<UserBigram>,
    enabled: bool,
    weights: PinyinWeights,
}

/// Chinese function words — Viterbi compositions made entirely of these
/// get penalised (e.g. "diyige → 的一个" is nonsense).
fn all_stopwords(word: &str) -> bool {
    const SET: &[char] = &[
        '的', '了', '是', '在', '和', '个', '不', '这', '也',
        '就', '都', '还', '要', '会', '能', '以', '及', '与',
        '着', '被', '把', '让', '向', '从', '到', '对', '为', '所',
        '而', '且', '或', '但', '于', '由', '因', '虽', '则',
        '一', '那', '它', '他', '她', '我', '你'
    ];
    !word.is_empty() && word.chars().all(|c| SET.contains(&c))
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
    pub session: f64,
    pub prefix: f64,
    pub phrase_book_prefix: f64,
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
    pub prefix_take: usize,
}

impl Default for PinyinWeights {
    fn default() -> Self {
        PinyinWeights {
            phrase_book: 1.0, large_dict: 0.95,
            viterbi_base: 0.3, viterbi_scale: 0.65,
            session: 0.5, prefix: 0.3,
            phrase_book_prefix: 0.85, jianpin: 0.70,
            single_syl_decay: 0.6, context_boost: 0.15,
            stopword_penalty: 0.5, confirm_bonus: 0.05, short_word_bonus: 0.02,
            large_dict_take: 96, viterbi_take: 48,
            jianpin_take: 8, prefix_take: 256,
        }
    }
}

impl PinyinFamily {
    pub fn new() -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(PhraseBook::default_phrases()),
            large_dict: Mutex::new(LargeDict::new()),
            bigram: Mutex::new(UserBigram::new()),
            enabled: true,
            weights: PinyinWeights::default(),
        }
    }

    pub fn with_weights(weights: PinyinWeights) -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(PhraseBook::default_phrases()),
            large_dict: Mutex::new(LargeDict::new()),
            bigram: Mutex::new(UserBigram::new()),
            enabled: true,
            weights,
        }
    }

    pub fn set_weights(&mut self, w: PinyinWeights) { self.weights = w; }

    pub fn with_phrase_book(phrase_book: PhraseBook) -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(phrase_book),
            large_dict: Mutex::new(LargeDict::new()),
            bigram: Mutex::new(UserBigram::new()),
            enabled: true,
            weights: PinyinWeights::default(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) { self.enabled = enabled; }

    pub fn record_bigram(&self, prev: &str, next: &str) {
        self.bigram.lock().unwrap().record(prev, next);
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
    }

    fn learn_phrase(&self, pinyin: &str, hanzi: &str) {
        let mut book = self.phrase_book.lock().unwrap();
        if !book.exact(pinyin).contains(&hanzi.to_string()) {
            book.insert(pinyin, hanzi);
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

    fn load_dict_bytes(&self, data: &[u8]) -> usize {
        // Load into LargeDict for large dictionaries, PhraseBook for small ones.
        // base.tsv (~5KB) goes to PhraseBook, rime-ice.tsv (~24MB) goes to LargeDict.
        if data.len() > 100_000 {
            self.large_dict.lock().unwrap().load_from_tsv_bytes(data)
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
        } else {
            // Large TSV files go to LargeDict for O(1) exact lookup.
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

        // ── Primary: Viterbi bigram model or single-syllable lookup ──
        if is_single_syllable {
            let words = dict.lookup(input);
            let total = words.len().max(1) as f64;
            for (i, word) in words.into_iter().enumerate() {
                out.push(ScoredCandidate {
                    text: word, family: "pinyin", source: "single",
                    raw_score: (1.0 - (i as f64 / total) * self.weights.single_syl_decay).clamp(0.0, 1.0),
                });
            }
        } else {
            let comps = dict.top_k_compositions(input, self.weights.viterbi_take);
            if !comps.is_empty() {
                let scores: Vec<f64> = comps.iter().map(|(s, _)| *s).collect();
                let min_s = scores.iter().cloned().fold(f64::INFINITY, f64::min);
                let max_s = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let range = (max_s - min_s).max(1.0);
                for (score, word) in comps {
                    let mut normalized = self.weights.viterbi_base + self.weights.viterbi_scale * ((score - min_s) / range);
                    // Penalise all-stopword compositions ONLY if not a real dict entry
                    // (e.g. diyige → 的一个 is nonsense, but yige → 一个 is a real word).
                    if all_stopwords(&word) {
                        let ld = self.large_dict.lock().unwrap();
                        if !ld.exact(input).contains(&word) {
                            normalized *= self.weights.stopword_penalty;
                        }
                    }
                    out.push(ScoredCandidate {
                        text: word, family: "pinyin", source: "viterbi",
                        raw_score: normalized.clamp(0.0, 1.0),
                    });
                }
            }
        }

        // ── LargeDict: fill gaps — words Viterbi/single didn't find ──
        if !is_single_syllable {
            let ld = self.large_dict.lock().unwrap();
            for w in ld.exact(input).into_iter().take(self.weights.large_dict_take) {
                if !out.iter().any(|c| c.text == w) {
                    out.push(ScoredCandidate { text: w, family: "pinyin", source: "dict", raw_score: self.weights.large_dict });
                }
            }
        }

        let mut session = inputx_pinyin::Session::new(&self.engine);
        for c in input.chars() { session.input_char(c); }
        for w in session.candidates() {
            let w = w.clone();
            if !out.iter().any(|c| c.text == w) {
                out.push(ScoredCandidate { text: w, family: "pinyin", source: "session", raw_score: self.weights.session });
            }
        }

        {
            let book = self.phrase_book.lock().unwrap();
            for w in book.exact(input) {
                out.retain(|c| c.text != w);
                out.push(ScoredCandidate { text: w, family: "pinyin", source: "phrase", raw_score: self.weights.phrase_book });
            }
        }
        // Jianpin (initials-only) fallback — "nh" → "你好".
        // Activated when input is 2-6 chars and looks like initials.
        if input.len() >= 2 && input.len() <= 6
            && !inputx_pinyin::is_valid_syllable(input)
            && input.chars().all(|c| c.is_ascii_lowercase())
        {
            let ld = self.large_dict.lock().unwrap();
            for w in ld.jianpin(input).into_iter().take(self.weights.jianpin_take) {
                if !out.iter().any(|c| c.text == w) {
                    out.push(ScoredCandidate { text: w, family: "pinyin", source: "jianpin", raw_score: self.weights.jianpin });
                }
            }
        }
        // ── Short-word preference (2-char words are most common targets) ──
        for c in &mut out {
            if c.text.chars().count() == 2 {
                c.raw_score = (c.raw_score + self.weights.short_word_bonus).min(1.0);
            }
        }

        if !out.is_empty() {
            out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
            return out;
        }

        for (_py, word) in dict.prefix(input).into_iter().take(self.weights.prefix_take) {
            if !out.iter().any(|c| c.text == word) {
                out.push(ScoredCandidate { text: word, family: "pinyin", source: "prefix", raw_score: self.weights.prefix });
            }
        }

        {
            let book = self.phrase_book.lock().unwrap();
            for w in book.prefix(input) {
                out.retain(|c| c.text != w);
                out.push(ScoredCandidate { text: w, family: "pinyin", source: "phrase_prefix", raw_score: self.weights.phrase_book_prefix });
            }
        }

        // Same post-merge boost as the early-return path.
        for c in &mut out {
            if c.text.chars().count() == 2 { c.raw_score = (c.raw_score + 0.02).min(1.0); }
        }
        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out
    }

    fn predict_with_context(&self, input: &str, ctx: &InputContext) -> Vec<ScoredCandidate> {
        let mut candidates = self.predict(input);
        if candidates.is_empty() { return candidates; }

        let dict = self.engine.dict();
        let bigram = self.bigram.lock().unwrap();
        let ctx_words: Vec<String> = ctx.recent_text.split_whitespace()
            .chain(std::iter::once(ctx.last_word.as_str()))
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
