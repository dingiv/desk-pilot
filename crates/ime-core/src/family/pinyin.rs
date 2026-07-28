//! PinyinFamily — Chinese full-pinyin prediction using inputx-pinyin's
//! embedded dictionary + bigram Viterbi composition + PhraseBook recall.

use super::{CandidateFamily, InputContext, ScoredCandidate};
use crate::large_dict::LargeDict;
use crate::phrase_book::PhraseBook;
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
    enabled: bool,
}

impl PinyinFamily {
    pub fn new() -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(PhraseBook::default_phrases()),
            large_dict: Mutex::new(LargeDict::new()),
            enabled: true,
        }
    }

    pub fn with_phrase_book(phrase_book: PhraseBook) -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(phrase_book),
            large_dict: Mutex::new(LargeDict::new()),
            enabled: true,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) { self.enabled = enabled; }

    pub fn learn_phrase(&self, pinyin: &str, hanzi: &str) {
        let mut book = self.phrase_book.lock().unwrap();
        if !book.exact(pinyin).contains(&hanzi.to_string()) {
            book.insert(pinyin, hanzi);
        }
    }

    pub fn record_pick(&self, pinyin: &str, word: &str) {
        self.engine.dict().record_pick(pinyin, word);
    }

    pub fn engine(&self) -> &inputx_pinyin::PinyinEngine { &self.engine }
    pub fn phrase_count(&self) -> usize { self.phrase_book.lock().unwrap().len() }
    pub fn large_dict_len(&self) -> usize { self.large_dict.lock().unwrap().len() }
}

impl Default for PinyinFamily {
    fn default() -> Self { Self::new() }
}

impl CandidateFamily for PinyinFamily {
    fn name(&self) -> &'static str { "pinyin" }
    fn priority(&self) -> u32 { 100 }
    fn enabled(&self) -> bool { self.enabled }
    fn top_n(&self) -> usize { 8 }

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
        if is_single_syllable {
            let words = dict.lookup(input);
            let total = words.len().max(1) as f64;
            for (i, word) in words.into_iter().enumerate() {
                out.push(ScoredCandidate {
                    text: word, family: "pinyin",
                    raw_score: (1.0 - (i as f64 / total) * 0.6).clamp(0.0, 1.0),
                });
            }
        } else {
            let comps = dict.top_k_compositions(input, 24);
            if !comps.is_empty() {
                let scores: Vec<f64> = comps.iter().map(|(s, _)| *s).collect();
                let min_s = scores.iter().cloned().fold(f64::INFINITY, f64::min);
                let max_s = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let range = (max_s - min_s).max(1.0);
                for (score, word) in comps {
                    let normalized = 0.3 + 0.65 * ((score - min_s) / range);
                    out.push(ScoredCandidate {
                        text: word, family: "pinyin",
                        raw_score: normalized.clamp(0.0, 1.0),
                    });
                }
            }
        }

        let mut session = inputx_pinyin::Session::new(&self.engine);
        for c in input.chars() { session.input_char(c); }
        for w in session.candidates() {
            let w = w.clone();
            if !out.iter().any(|c| c.text == w) {
                out.push(ScoredCandidate { text: w, family: "pinyin", raw_score: 0.5 });
            }
        }

        {
            let book = self.phrase_book.lock().unwrap();
            for w in book.exact(input) {
                out.retain(|c| c.text != w);
                out.push(ScoredCandidate { text: w, family: "pinyin", raw_score: 1.0 });
            }
        }
        // LargeDict: 900K+ entries, O(1) exact match. Lower score than
        // PhraseBook so user-custom phrases always win.
        {
            let ld = self.large_dict.lock().unwrap();
            for w in ld.exact(input).into_iter().take(8) {
                if !out.iter().any(|c| c.text == w) {
                    out.push(ScoredCandidate { text: w, family: "pinyin", raw_score: 0.95 });
                }
            }
        }

        if !out.is_empty() {
            out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
            return out;
        }

        for (_py, word) in dict.prefix(input).into_iter().take(72) {
            if !out.iter().any(|c| c.text == word) {
                out.push(ScoredCandidate { text: word, family: "pinyin", raw_score: 0.3 });
            }
        }

        {
            let book = self.phrase_book.lock().unwrap();
            for w in book.prefix(input) {
                out.retain(|c| c.text != w);
                out.push(ScoredCandidate { text: w, family: "pinyin", raw_score: 0.85 });
            }
        }

        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out
    }

    fn predict_with_context(&self, input: &str, ctx: &InputContext) -> Vec<ScoredCandidate> {
        let mut candidates = self.predict(input);
        let last = &ctx.last_word;
        if last.is_empty() || candidates.is_empty() || last.chars().count() != 1 {
            return candidates;
        }
        let last_char = last.chars().next().unwrap();
        let dict = self.engine.dict();
        for c in &mut candidates {
            if let Some(cand_char) = c.text.chars().next() {
                for py_last in inputx_pinyin::char_to_pinyin(last_char) {
                    for py_cand in inputx_pinyin::char_to_pinyin(cand_char) {
                        let combined_py = format!("{py_last}{py_cand}");
                        let combined_word: String = [last_char, cand_char].into_iter().collect();
                        if dict.lookup(&combined_py).iter().any(|w| *w == combined_word) {
                            c.raw_score = (c.raw_score + 0.15).min(1.0);
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
