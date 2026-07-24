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
        session.candidates().iter().take(16).cloned().collect()
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
