//! inputx-pinyin adapter — implements ime-core's [`PinyinEngine`] trait.
//!
//! `inputx-pinyin` ships an embedded dictionary (core + bigrams + trigrams), so no
//! runtime download is needed. The engine is constructed once and reused; per-query
//! a short-lived [`Session`] borrows it (Session can't outlive the engine, so it's
//! kept inside the `candidates` call scope).

use ime_core::PinyinEngine;

/// Wraps the inputx-pinyin engine. Clone is cheap-ish (shares dict data).
pub struct InputxPinyin(inputx_pinyin::PinyinEngine);

impl InputxPinyin {
    pub fn new() -> Self {
        Self(inputx_pinyin::PinyinEngine::new())
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
        let mut session = inputx_pinyin::Session::new(&self.0);
        for c in pinyin.chars() {
            session.input_char(c);
        }
        session.candidates().to_vec()
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
        assert!(cands[0].contains("你好"), "top candidate was {:?}, expected 你好", cands[0]);
    }

    #[test]
    fn empty_pinyin_returns_empty() {
        let engine = InputxPinyin::new();
        assert!(engine.candidates("").is_empty());
    }
}
