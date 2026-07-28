//! Custom phrase book — supplements inputx-pinyin with user-configured
//! pinyin→hanzi mappings. Phrases here are checked AFTER the main pinyin
//! engine and inserted at the top of the candidate list.
//!
//! Format (JSON):
//! ```json
//! [
//!   {"pinyin": "xiayige",  "text": "下一个"},
//!   {"pinyin": "zheshi",   "text": "这是"},
//!   {"pinyin": "xiayig",   "text": "下一个"}
//! ]
//! ```

use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct PhraseBook {
    /// pinyin (no spaces) → list of hanzi phrases
    entries: HashMap<String, Vec<String>>,
    /// All pinyin keys, longest first — for prefix matching during typing.
    keys_by_len: Vec<String>,
}

impl PhraseBook {
    pub fn new() -> Self {
        PhraseBook::default()
    }

    /// Number of phrases in the book.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Load from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct Entry {
            pinyin: String,
            text: String,
        }

        let raw: Vec<Entry> = serde_json::from_str(json)?;
        let mut book = PhraseBook::new();
        for e in raw {
            book.insert(&e.pinyin, &e.text);
        }
        Ok(book)
    }

    /// Add one phrase. If the pinyin key already exists, appends to its list.
    pub fn insert(&mut self, pinyin: &str, text: &str) {
        self.entries
            .entry(pinyin.to_string())
            .or_default()
            .push(text.to_string());
    }

    /// Exact match — only returns candidates when the full pinyin matches.
    pub fn exact(&self, pinyin: &str) -> Vec<String> {
        self.entries.get(pinyin).cloned().unwrap_or_default()
    }

    /// Prefix match — returns candidates whose pinyin starts with `prefix`.
    /// Useful during typing: "xiay" → matches "xiayige" → shows "下一个".
    pub fn prefix(&self, prefix: &str) -> Vec<String> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (py, texts) in &self.entries {
            if py.starts_with(prefix) {
                for t in texts {
                    if !out.contains(t) {
                        out.push(t.clone());
                    }
                }
            }
        }
        out
    }

    /// Rebuild the sorted key list (call after bulk insert).
    pub fn reindex(&mut self) {
        let mut keys: Vec<String> = self.entries.keys().cloned().collect();
        keys.sort_by(|a, b| b.len().cmp(&a.len()));
        self.keys_by_len = keys;
    }

    /// Load phrases from a TSV file (tab-separated: `pinyin\tword`).
    /// Lines starting with `#` or `---` are skipped (YAML header).
    /// Empty lines are skipped.
    /// RIME dictionary format: `T恤\tTxu` — reverse order of our format.
    /// We auto-detect: if the first tab-separated field is hanzi-heavy,
    /// we swap the columns.
    pub fn load_from_tsv(&mut self, path: &str) -> std::io::Result<usize> {
        let content = std::fs::read_to_string(path)?;
        let mut count = 0;
        let mut first_line = true;
        let mut swap = false;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("---") {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            if first_line {
                // Auto-detect: if first field contains hanzi, it's RIME format
                // (word\tpinyin) and we need to swap.
                swap = parts[0].chars().any(|c| c as u32 > 127);
                first_line = false;
            }
            let (pinyin, word) = if swap {
                (parts[1].to_string(), parts[0].to_string())
            } else {
                (parts[0].to_string(), parts[1].to_string())
            };
            if !pinyin.is_empty() && !word.is_empty() {
                self.insert(&pinyin, &word);
                count += 1;
            }
        }
        tracing::info!(count, path, "loaded external dictionary");
        Ok(count)
    }

    /// Load phrases from raw TSV bytes (for compile-time embedded dicts).
    pub fn load_from_tsv_bytes(&mut self, data: &[u8]) -> usize {
        let mut count = 0;
        if let Ok(s) = std::str::from_utf8(data) {
            for line in s.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') { continue; }
                if let Some((pinyin, word)) = line.split_once('\t') {
                    if !pinyin.is_empty() && !word.is_empty() {
                        self.insert(pinyin, word);
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// Load from a JSON string (existing method).
    pub fn load_from_json_str(&mut self, json: &str) -> Result<usize, serde_json::Error> {
        let book = PhraseBook::from_json(json)?;
        let count = book.len();
        for (pinyin, words) in book.entries {
            for word in words {
                self.insert(&pinyin, &word);
            }
        }
        Ok(count)
    }

    /// Built-in default phrases that supplement inputx-pinyin's dictionary.
    pub fn default_phrases() -> Self {
        let json = r#"[
            {"pinyin": "xiayige", "text": "下一个"},
            {"pinyin": "zheshi",  "text": "这是"},
            {"pinyin": "xiayig",  "text": "下一个"},
            {"pinyin": "haode",   "text": "好的"},
            {"pinyin": "zhidao",  "text": "知道"},
            {"pinyin": "xianzaishuo","text": "现在说"}
        ]"#;
        PhraseBook::from_json(json).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        let mut book = PhraseBook::new();
        book.insert("xiayige", "下一个");
        book.insert("zheshi", "这是");
        assert_eq!(book.exact("xiayige"), vec!["下一个"]);
        assert_eq!(book.exact("noth"), Vec::<String>::new());
    }

    #[test]
    fn prefix_during_typing() {
        let mut book = PhraseBook::new();
        book.insert("xiayige", "下一个");
        book.insert("xiayig", "下一个"); // also match intermediate state
        // During typing "xiayig", should find "下一个"
        let r = book.prefix("xiayig");
        assert!(r.contains(&"下一个".to_string()), "expected 下一个 in {:?}", r);
    }

    #[test]
    fn load_from_json() {
        let json = r#"[{"pinyin":"xiayige","text":"下一个"},{"pinyin":"zheshi","text":"这是"}]"#;
        let book = PhraseBook::from_json(json).unwrap();
        assert_eq!(book.exact("xiayige"), vec!["下一个"]);
        assert_eq!(book.exact("zheshi"), vec!["这是"]);
    }

    #[test]
    fn multiple_phrases_same_pinyin() {
        let mut book = PhraseBook::new();
        book.insert("ceshi", "测试");
        book.insert("ceshi", "侧室");
        let r = book.exact("ceshi");
        assert!(r.contains(&"测试".to_string()));
        assert!(r.contains(&"侧室".to_string()));
    }
}
