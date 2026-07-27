//! Pinyin engine — powered by `inputx-pinyin` v1.4 with its embedded
//! 3.9 MB FST dictionary + bigram model for Viterbi composition.
//!
//! Query strategy (layered, each step feeds into the next):
//! 1. **Viterbi composition** — `dict().top_k_compositions()` uses bigrams
//!    to decompose `xiayige` → `下+一+个` → "下一个". Best quality.
//! 2. **Session** — `Session::candidates()` returns dictionary entries for
//!    the input-as-written (phrase-level lookups).
//! 3. **PhraseBook** — user-custom pinyin→hanzi mappings inserted at front.
//! 4. **Prefix fallback** — `dict().prefix()` for incomplete final syllable
//!    (e.g. "zhengz" → 政治/正在/挣扎).
//! 5. **PhraseBook prefix** — custom prefix matches.

use std::sync::Mutex;

use crate::phrase_book::PhraseBook;
use crate::PinyinEngine;

pub struct InputxPinyin {
    engine: inputx_pinyin::PinyinEngine,
    phrase_book: Mutex<PhraseBook>,
}

impl InputxPinyin {
    pub fn new() -> Self {
        Self {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(PhraseBook::default_phrases()),
        }
    }

    pub fn with_phrase_book(phrase_book: PhraseBook) -> Self {
        Self {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(phrase_book),
        }
    }
}

impl Default for InputxPinyin {
    fn default() -> Self {
        Self::new()
    }
}

impl PinyinEngine for InputxPinyin {
    fn first_syllable(&self, pinyin: &str) -> Option<String> {
        let max = pinyin.len().min(6);
        for len in (1..=max).rev() {
            let candidate = &pinyin[..len];
            if inputx_pinyin::is_valid_syllable(candidate) {
                return Some(candidate.to_string());
            }
        }
        None
    }

    fn record_pick(&self, pinyin: &str, word: &str) {
        self.engine.dict().record_pick(pinyin, word);
    }

    fn learn_phrase(&self, pinyin: &str, hanzi: &str) {
        let mut book = self.phrase_book.lock().unwrap();
        let existing = book.exact(pinyin);
        if !existing.contains(&hanzi.to_string()) {
            book.insert(pinyin, hanzi);
            eprintln!("[PhraseBook] learned: {pinyin} → {hanzi}");
        }
    }

    fn candidates(&self, pinyin: &str) -> Vec<String> {
        if pinyin.is_empty() {
            return Vec::new();
        }

        let dict = self.engine.dict();

        // ── Layer 1: Viterbi bigram composition (best quality) ──────
        let mut result: Vec<String> = dict
            .top_k_compositions(pinyin, 24)
            .into_iter()
            .map(|(_score, word)| word)
            .collect();

        // ── Layer 2: Session phrase-level lookup ────────────────────
        let mut session = inputx_pinyin::Session::new(&self.engine);
        for c in pinyin.chars() {
            session.input_char(c);
        }
        for w in session.candidates() {
            let w = w.clone();
            if !result.contains(&w) {
                result.push(w);
            }
        }

        // ── Layer 3: User PhraseBook exact match — move to front ─
        for w in self.phrase_book.lock().unwrap().exact(pinyin) {
            result.retain(|r| r != &w);
            result.insert(0, w);
        }

        // Early return if we already have results.
        if !result.is_empty() {
            result.truncate(72);
            return result;
        }

        // ── Layer 4: Prefix fallback (incomplete final syllable) ───
        let mut prefix: Vec<String> = dict
            .prefix(pinyin)
            .into_iter()
            .map(|(_py, word)| word)
            .take(72)
            .collect();

        // ── Layer 5: PhraseBook prefix — move to front ────────────
        for w in self.phrase_book.lock().unwrap().prefix(pinyin) {
            prefix.retain(|r| r != &w);
            prefix.insert(0, w);
        }

        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_nihao_yields_hello() {
        let engine = InputxPinyin::new();
        let cands = engine.candidates("nihao");
        assert!(!cands.is_empty(), "expected candidates for 'nihao'");
        assert!(
            cands[0].contains("你好"),
            "top candidate was {:?}, expected 你好",
            cands[0]
        );
    }

    #[test]
    fn empty_pinyin_returns_empty() {
        let engine = InputxPinyin::new();
        assert!(engine.candidates("").is_empty());
    }

    #[test]
    fn composition_xiayige() {
        let e = InputxPinyin::new();
        let cands = e.candidates("xiayige");
        assert!(
            cands.iter().any(|c| c == "下一个"),
            "expected 下一个 in {:?}",
            cands
        );
    }

    #[test]
    fn composition_zheshi() {
        let e = InputxPinyin::new();
        let cands = e.candidates("zheshi");
        assert!(
            cands.iter().any(|c| c == "这是"),
            "expected 这是 in {:?}",
            cands
        );
    }

    #[test]
    fn composition_kuifa_multiple() {
        let e = InputxPinyin::new();
        let cands = e.candidates("kuifa");
        assert!(
            cands.len() >= 2,
            "kuifa should have multiple candidates, got {:?}",
            cands
        );
        assert!(cands.iter().any(|c| c == "匮乏"));
    }

    #[test]
    fn prefix_works_for_incomplete_syllable() {
        let e = InputxPinyin::new();
        let cands = e.candidates("kuif");
        assert!(
            !cands.is_empty(),
            "kuif (incomplete) should have prefix matches"
        );
    }

    #[test]
    fn composition_woyaochifan() {
        let e = InputxPinyin::new();
        let cands = e.candidates("woyaochifan");
        assert!(
            cands.iter().any(|c| c == "我要吃饭"),
            "expected 我要吃饭 in {:?}",
            cands
        );
    }
}
