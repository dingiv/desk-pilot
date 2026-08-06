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

use super::initials_from_pinyin;

#[derive(Debug, Clone)]
struct Phrase {
    text: String,
    order: i32, // 0 = highest priority (fcitx5 CustomPhrase convention)
    /// 使用次数(选中即 +1)—— phrase 词按使用频率参与排名,
    /// 而不是所有自造词共享一个固定分。
    count: u32,
}

#[derive(Debug, Clone)]
pub struct PhraseBook {
    /// pinyin (no spaces) → list of hanzi phrases
    entries: HashMap<String, Vec<Phrase>>,
    /// Initials index: "lzm" → ["李正明", ...] for jianpin recall.
    initials_index: HashMap<String, Vec<Phrase>>,
    /// All pinyin keys, longest first — for prefix matching during typing.
    keys_by_len: Vec<String>,
}

impl Default for PhraseBook {
    fn default() -> Self {
        PhraseBook {
            entries: HashMap::new(),
            initials_index: HashMap::new(),
            keys_by_len: Vec::new(),
        }
    }
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

    /// Add one phrase with default highest priority (order=0).
    pub fn insert(&mut self, pinyin: &str, text: &str) {
        self.insert_with_order(pinyin, text, 0);
    }

    /// Add one phrase with a specific priority order (0 = highest).
    /// `count` = 已有使用次数(从持久化恢复时为历史值,新词为 0)。
    pub fn insert_with_order_count(&mut self, pinyin: &str, text: &str, order: i32, count: u32) {
        let phrase = Phrase { text: text.to_string(), order, count };
        // Full pinyin index.
        let list = self.entries.entry(pinyin.to_string()).or_default();
        list.retain(|p| p.text != text);
        list.push(phrase.clone());
        list.sort_by_key(|p| p.order);
        // Initials index for jianpin recall (lzm → 李正明).
        let initials = initials_from_pinyin(pinyin);
        if initials.len() >= 2 {
            let init_list = self.initials_index.entry(initials).or_default();
            init_list.retain(|p| p.text != text);
            init_list.push(phrase);
            init_list.sort_by_key(|p| p.order);
        }
    }

    /// Add one phrase with default highest priority (order=0), count starts at 1.
    pub fn insert_with_order(&mut self, pinyin: &str, text: &str, order: i32) {
        self.insert_with_order_count(pinyin, text, order, 1);
    }

    /// 用户再次选中:使用次数 +1(维持 order,不重复插入)。
    pub fn bump_count(&mut self, pinyin: &str, text: &str) {
        let mut bumped = false;
        if let Some(list) = self.entries.get_mut(pinyin) {
            if let Some(p) = list.iter_mut().find(|p| p.text == text) {
                p.count = p.count.saturating_add(1);
                bumped = true;
            }
        }
        if !bumped { return; }
        // Initials 索引同步。
        let initials = initials_from_pinyin(pinyin);
        if initials.len() >= 2 {
            if let Some(list) = self.initials_index.get_mut(&initials) {
                if let Some(p) = list.iter_mut().find(|p| p.text == text) {
                    p.count = p.count.saturating_add(1);
                }
            }
        }
    }

    /// 某个 phrase 的使用次数(0 = 不在词本)。
    pub fn count(&self, pinyin: &str, text: &str) -> u32 {
        self.entries.get(pinyin)
            .and_then(|list| list.iter().find(|p| p.text == text))
            .map(|p| p.count)
            .unwrap_or(0)
    }

    /// Exact match — candidates sorted by order (0 = highest first).
    pub fn exact(&self, pinyin: &str) -> Vec<String> {
        let mut list: Vec<&Phrase> = self.entries.get(pinyin)
            .map(|v| v.iter().collect()).unwrap_or_default();
        list.sort_by_key(|p| p.order);
        list.into_iter().map(|p| p.text.clone()).collect()
    }

    /// Initials (jianpin) match — candidates sorted by order.
    /// "lzm" → ["李正明", ...] (from previously learned lizhengming→李正明).
    pub fn by_initials(&self, initials: &str) -> Vec<String> {
        if initials.is_empty() || initials.len() < 2 { return Vec::new(); }
        let mut list: Vec<&Phrase> = self.initials_index.get(initials)
            .map(|v| v.iter().collect()).unwrap_or_default();
        list.sort_by_key(|p| p.order);
        list.into_iter().map(|p| p.text.clone()).collect()
    }

    /// Prefix match — candidates sorted by order (0 = highest first).
    pub fn prefix(&self, prefix: &str) -> Vec<String> {
        if prefix.is_empty() { return Vec::new(); }
        let mut all: Vec<&Phrase> = Vec::new();
        for (py, texts) in &self.entries {
            if py.starts_with(prefix) {
                all.extend(texts.iter());
            }
        }
        all.sort_by_key(|p| p.order);
        let mut out = Vec::new();
        for p in all {
            if !out.contains(&p.text) { out.push(p.text.clone()); }
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
        for (pinyin, phrases) in book.entries {
            for p in phrases {
                self.insert_with_order(&pinyin, &p.text, p.order);
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

    #[test]
    fn initials_lookup_lzm() {
        let mut book = PhraseBook::new();
        book.insert("lizhengming", "李正明");
        // Full pinyin exact match.
        assert_eq!(book.exact("lizhengming"), vec!["李正明"]);
        // Initials match: lzm → 李正明.
        let r = book.by_initials("lzm");
        assert!(r.contains(&"李正明".to_string()),
            "lzm should find 李正明, got {:?}", r);
    }

    #[test]
    fn initials_lookup_multiple() {
        let mut book = PhraseBook::new();
        book.insert("lizhengming", "李正明");
        book.insert("lizhongming", "李中明");
        let r = book.by_initials("lzm");
        assert_eq!(r.len(), 2, "lzm should find both 李正明 and 李中明, got {:?}", r);
    }
}
