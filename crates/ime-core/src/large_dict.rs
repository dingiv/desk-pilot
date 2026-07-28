//! LargeDict — high-capacity pinyin→hanzi dictionary for exact lookup only.
//!
//! Unlike [`PhraseBook`](crate::phrase_book::PhraseBook) (which supports both
//! exact and prefix matching for a small set of user phrases), LargeDict is
//! optimized for ~1M entries from external sources like rime-ice. It only
//! supports **exact pinyin match** via O(1) HashMap lookup — no prefix scan.
//!
//! Loading a 24MB TSV file (~900K entries) takes ~200-400ms and uses
//! ~50-80MB of memory. This is a one-time cost at engine startup.

use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct LargeDict {
    /// pinyin → list of matching hanzi words
    entries: HashMap<String, Vec<String>>,
}

impl LargeDict {
    pub fn new() -> Self {
        LargeDict::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Exact pinyin lookup — O(1).
    pub fn exact(&self, pinyin: &str) -> Vec<String> {
        self.entries.get(pinyin).cloned().unwrap_or_default()
    }

    /// Load from a TSV file (tab-separated: `pinyin\tword`).
    /// Skips lines starting with `#`. Returns the number of entries loaded.
    pub fn load_from_tsv_file(&mut self, path: &str) -> std::io::Result<usize> {
        let content = std::fs::read_to_string(path)?;
        let count = self.load_from_tsv_bytes(content.as_bytes());
        Ok(count)
    }

    /// Load from TSV bytes (for compile-time embedded dicts).
    pub fn load_from_tsv_bytes(&mut self, data: &[u8]) -> usize {
        let mut count = 0;
        let start = std::time::Instant::now();
        if let Ok(s) = std::str::from_utf8(data) {
            for line in s.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((pinyin, word)) = line.split_once('\t') {
                    if !pinyin.is_empty() && !word.is_empty() {
                        // Normalize: rime-ice uses space-separated syllables
                        // ("shu ru"), but our input is concatenated ("shuru").
                        let key = pinyin.replace(' ', "");
                        self.entries
                            .entry(key)
                            .or_default()
                            .push(word.to_string());
                        count += 1;
                    }
                }
            }
        }
        let elapsed = start.elapsed();
        tracing::info!(
            count,
            keys = self.entries.len(),
            ms = elapsed.as_millis(),
            "large dict loaded from bytes"
        );
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_lookup() {
        let mut dict = LargeDict::new();
        let data = "xiayige\t\u{4e0b}\u{4e00}\u{4e2a}\nzheshi\t\u{8fd9}\u{662f}\n";
        dict.load_from_tsv_bytes(data.as_bytes());
        assert_eq!(dict.exact("xiayige"), vec!["\u{4e0b}\u{4e00}\u{4e2a}"]); // 下一个
        assert_eq!(dict.exact("zheshi"), vec!["\u{8fd9}\u{662f}"]); // 这是
        assert!(dict.exact("unknown").is_empty());
    }

    #[test]
    fn empty_lookup() {
        let dict = LargeDict::new();
        assert!(dict.exact("anything").is_empty());
    }
}
