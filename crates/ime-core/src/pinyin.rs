//! Pinyin engine — the built-in pinyin-to-hanzi converter used by the dispatcher's
//! PinyinPath. Powered by the community `inputx-pinyin` crate (embedded dictionary).
//!
//! Implements the [`PinyinEngine`] trait so the dispatcher can call `candidates()` on
//! every keystroke. Tests inject a [`StubPinyin`] to avoid the dictionary overhead.

use crate::PinyinEngine;

/// The real pinyin engine (inputx-pinyin). Construct once and reuse — the dictionary
/// is loaded at construction time and shared across all queries.
pub struct InputxPinyin(inputx_pinyin::PinyinEngine);

impl InputxPinyin {
    pub fn new() -> Self {
        Self(inputx_pinyin::PinyinEngine::with_fuzzy(inputx_pinyin::FuzzyConfig::permissive()))
    }
}

impl Default for InputxPinyin {
    fn default() -> Self {
        Self::new()
    }
}

impl PinyinEngine for InputxPinyin {
    fn candidates(&self, pinyin: &str) -> Vec<String> {
        if pinyin.is_empty() {
            return Vec::new();
        }
        // Step 1: exact syllable match (with fuzzy expansion).
        let mut session = inputx_pinyin::Session::new(&self.0);
        for c in pinyin.chars() {
            session.input_char(c);
        }
        let exact = session.candidates();
        if !exact.is_empty() {
            return exact.iter().take(72).cloned().collect();
        }
        // Step 2: prefix fallback — the input has a trailing incomplete syllable
        // (e.g. "zhengz"). The dict scans all words whose pinyin starts with this
        // prefix, which is how "zhengz" finds 政治(zhengzhi)/正在(zhengzai)/挣扎(zhengzha).
        let prefix: Vec<String> = self.0.dict().prefix(pinyin)
            .into_iter()
            .map(|(_py, word)| word)
            .take(72)
            .collect();
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
        assert!(cands[0].contains("你好"),
            "top candidate was {:?}, expected 你好", cands[0]);
    }

    #[test]
    fn empty_pinyin_returns_empty() {
        let engine = InputxPinyin::new();
        assert!(engine.candidates("").is_empty());
    }
}
