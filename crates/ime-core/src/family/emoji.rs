//! EmojiFamily — emoji prediction as a first-class family in the unified scorer.
//!
//! Ranks alongside pinyin + english (中英混输 competition): every emoji has
//! keyword groups — English name, pinyin, and hanzi — so `smile`, `weixiao`,
//! or `微` all surface 😊 as a candidate. Prefix match shows the emoji while
//! the user is still typing; exact match scores 1.0.
//!
//! No special trigger prefix: the family participates in the normal candidate
//! competition, and its priority (default 60, below english 70) keeps emoji
//! from drowning out text candidates.
//!
//! ## Dictionary layers (like EnglishFamily)
//! 1. **base** — the curated table below, compiled into the binary (backstop);
//! 2. **external** — a large keyword table generated from Unicode CLDR
//!    annotations (`assets/dict/emoji.tsv`, via `scripts/fetch_emoji.sh`),
//!    loaded at startup: `keyword<TAB>emoji` per line, `#` comments allowed;
//! 3. **user** — the user's own mapping (`emoji_user.tsv`).
//!
//! Later loads override earlier ones per keyword (user wins over external,
//! external over base); the entry POSITION is kept so ranking stays stable.

use std::sync::Mutex;

use super::{CandidateFamily, ScoredCandidate};

/// One emoji and its keywords. `keys` are all prefix-matched against the input;
/// duplicate keywords across entries are fine — the first hit per emoji wins.
struct EmojiEntry {
    emoji: &'static str,
    keys: &'static [&'static str],
}

/// Built-in table — deliberately curated to common, unambiguous terms so a
/// casual prefix ("he" → heart) doesn't flood the candidate list.
const EMOJI_TABLE: &[EmojiEntry] = &[
    EmojiEntry { emoji: "😊", keys: &["smile", "smiling", "weixiao", "微笑", "笑脸"] },
    EmojiEntry { emoji: "😂", keys: &["laugh", "lol", "daxiao", "大笑", "笑哭"] },
    EmojiEntry { emoji: "😢", keys: &["cry", "kulei", "哭", "流泪"] },
    EmojiEntry { emoji: "❤️", keys: &["heart", "love", "aixin", "爱心"] },
    EmojiEntry { emoji: "👍", keys: &["ok", "good", "zan", "赞", "可以"] },
    EmojiEntry { emoji: "👏", keys: &["clap", "guzhang", "鼓掌"] },
    EmojiEntry { emoji: "🔥", keys: &["fire", "huo", "火", "牛"] },
    EmojiEntry { emoji: "⭐", keys: &["star", "xingxing", "星星"] },
    EmojiEntry { emoji: "✅", keys: &["check", "done", "dui", "对", "完成"] },
    EmojiEntry { emoji: "🤔", keys: &["think", "sikao", "思考"] },
    EmojiEntry { emoji: "😎", keys: &["cool", "ku", "酷"] },
    EmojiEntry { emoji: "😠", keys: &["angry", "shengqi", "生气", "愤怒"] },
    EmojiEntry { emoji: "😮", keys: &["surprised", "jingya", "惊讶"] },
    EmojiEntry { emoji: "😉", keys: &["wink", "zhayan", "眨眼"] },
    EmojiEntry { emoji: "😘", keys: &["kiss", "qinqin", "亲亲"] },
    EmojiEntry { emoji: "😴", keys: &["sleep", "shuijiao", "睡觉", "困"] },
    EmojiEntry { emoji: "😅", keys: &["sweat", "ganga", "尴尬", "汗"] },
    EmojiEntry { emoji: "🙏", keys: &["pray", "qidao", "祈祷", "拜托"] },
    EmojiEntry { emoji: "👋", keys: &["wave", "zaijian", "再见", "挥手"] },
    EmojiEntry { emoji: "🎉", keys: &["party", "celebrate", "qingzhu", "庆祝", "派对"] },
    EmojiEntry { emoji: "🎁", keys: &["gift", "liwu", "礼物"] },
    EmojiEntry { emoji: "🚀", keys: &["rocket", "huojian", "火箭"] },
    EmojiEntry { emoji: "☀️", keys: &["sun", "sunny", "taiyang", "太阳", "晴天"] },
    EmojiEntry { emoji: "🌙", keys: &["moon", "yueliang", "月亮"] },
    EmojiEntry { emoji: "☕", keys: &["coffee", "kafei", "咖啡"] },
    EmojiEntry { emoji: "🍵", keys: &["tea", "cha", "茶"] },
    EmojiEntry { emoji: "🐱", keys: &["cat", "mao", "猫"] },
    EmojiEntry { emoji: "🐶", keys: &["dog", "gou", "狗"] },
];

pub struct EmojiFamily {
    enabled: bool,
    /// (keyword, emoji) in insertion order — base table first, then loaded
    /// dicts. Merging keeps the original position of an existing keyword
    /// (stable ranking) while replacing its emoji (later load wins).
    entries: Mutex<Vec<(String, String)>>,
}

impl EmojiFamily {
    pub fn new() -> Self {
        let mut entries = Vec::new();
        for e in EMOJI_TABLE {
            for k in e.keys {
                entries.push((k.to_string(), e.emoji.to_string()));
            }
        }
        EmojiFamily { enabled: true, entries: Mutex::new(entries) }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Merge loaded rows into the table. A keyword already present keeps its
    /// position but gets the new emoji (later loads override earlier ones).
    fn merge(&self, rows: Vec<(String, String)>) -> usize {
        let mut entries = self.entries.lock().unwrap();
        let mut count = 0;
        for (k, e) in rows {
            match entries.iter_mut().find(|(ek, _)| *ek == k) {
                Some(slot) => slot.1 = e,
                None => entries.push((k, e)),
            }
            count += 1;
        }
        count
    }

    /// Load a keyword table from TSV: `keyword<TAB>emoji` per line, `#`
    /// comment lines and blank lines skipped. Shared by the external dict and
    /// the user dict — call order decides precedence (later wins).
    pub fn load_tsv(&self, path: &str) -> std::io::Result<usize> {
        let data = std::fs::read_to_string(path)?;
        let rows = parse_emoji_tsv(&data);
        let n = self.merge(rows);
        if n > 0 {
            tracing::info!(count = n, path, "emoji: loaded keyword table");
        }
        Ok(n)
    }
}

impl Default for EmojiFamily {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse `keyword<TAB>emoji` lines; `#`-prefixed and blank lines skipped.
/// Malformed lines (missing a column) are dropped.
fn parse_emoji_tsv(data: &str) -> Vec<(String, String)> {
    data.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split('\t');
            let kw = it.next()?.trim();
            let emoji = it.next()?.trim();
            if kw.is_empty() || emoji.is_empty() {
                None
            } else {
                Some((kw.to_string(), emoji.to_string()))
            }
        })
        .collect()
}

impl CandidateFamily for EmojiFamily {
    fn name(&self) -> &'static str {
        "emoji"
    }

    fn priority(&self) -> u32 {
        60
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn top_n(&self) -> usize {
        4
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        // Single-char input is skipped: with a large CLDR table, "a"/"h" would
        // match hundreds of generic keywords and flood the candidate list.
        if input.chars().count() < 2 {
            return Vec::new();
        }
        let entries = self.entries.lock().unwrap();
        let mut out = Vec::new();
        let input_len = input.chars().count();
        for (kw, emoji) in entries.iter() {
            if kw.starts_with(input) {
                let exact = kw == input;
                // 前缀分 0.6 + 距离衰减:用户打中文途中(weishenm →
                // weishenme→🤌)emoji 只前缀命中时,应明显低于拼音家族的
                // 联想候选(0.5+),不插队。剩余 ≤2 字符视为"马上打完"
                // 不衰减(与拼音前缀联想同规则),超出按 0.85^剩余 衰减。
                //
                // ≤2 字母的短关键词(cd→📀、ok→👍)即使完整命中也降为
                // 前缀档:两字母输入几乎总是中文简拼(承担/程度/成都)或
                // 常用英文缩写,emoji 不该压过它们。
                let short_kw = kw.chars().count() <= 2;
                let score = if exact && !short_kw {
                    1.0
                } else {
                    let excess = (kw.chars().count())
                        .saturating_sub(input_len)
                        .saturating_sub(2) as f64;
                    0.6 * 0.85_f64.powf(excess)
                };
                out.push(ScoredCandidate {
                    text: emoji.clone(),
                    family: "emoji",
                    source: if exact { "exact" } else { "prefix" },
                    raw_score: score,
                });
            }
        }
        drop(entries);
        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        self.load_tsv(path)
    }

    fn load_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.load_tsv(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_english_keyword() {
        let fam = EmojiFamily::new();
        let cands = fam.predict("smile");
        assert!(cands.iter().any(|c| c.text == "😊"), "{cands:?}");
        assert_eq!(cands[0].raw_score, 1.0, "exact match scores 1.0");
    }

    #[test]
    fn prefix_english_keyword() {
        let fam = EmojiFamily::new();
        let cands = fam.predict("smil");
        assert!(cands.iter().any(|c| c.text == "😊"), "{cands:?}");
        // smil→smile 剩余 1(≤2 免衰减)→ 前缀基础分 0.6。
        assert_eq!(cands[0].raw_score, 0.6, "prefix match scores 0.6");
    }

    #[test]
    fn single_char_input_skipped() {
        // With the large CLDR table a 1-char input would match hundreds of
        // generic keywords — the family stays silent until ≥2 chars.
        let fam = EmojiFamily::new();
        assert!(fam.predict("a").is_empty());
        assert!(fam.predict("h").is_empty());
        assert!(fam.predict("微").is_empty(), "even hanzi needs 2 chars");
    }

    #[test]
    fn load_external_table_extends_base() {
        let dir = std::env::temp_dir().join(format!("emoji-ext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("emoji.tsv");
        std::fs::write(&path, "# comment\nrocket_emoji\t🚀\n外星人\t👽\n").unwrap();
        let fam = EmojiFamily::new();
        assert!(fam.predict("外星人").is_empty(), "not in base");
        let n = fam.load_tsv(path.to_str().unwrap()).unwrap();
        assert_eq!(n, 2);
        assert!(fam.predict("外星人").iter().any(|c| c.text == "👽"), "external keyword works");
        assert!(fam.predict("rocket_e").iter().any(|c| c.text == "🚀"), "english keyword works");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn later_load_overrides_earlier_per_keyword() {
        // user 词典覆盖 external 覆盖 base:同关键词后加载的生效(位置保持)。
        let dir = std::env::temp_dir().join(format!("emoji-ovr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ext = dir.join("ext.tsv");
        std::fs::write(&ext, "smile\t😀\n").unwrap(); // override base smile → 😊
        let user = dir.join("user.tsv");
        std::fs::write(&user, "smile\t🥰\n").unwrap(); // override ext → 🥰
        let fam = EmojiFamily::new();
        fam.load_tsv(ext.to_str().unwrap()).unwrap();
        assert!(fam.predict("smile").iter().any(|c| c.text == "😀"), "external overrides base");
        fam.load_tsv(user.to_str().unwrap()).unwrap();
        let cands = fam.predict("smile");
        assert!(cands.iter().any(|c| c.text == "🥰"), "user overrides external: {cands:?}");
        assert!(!cands.iter().any(|c| c.text == "😊"), "base replaced: {cands:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_lines_skipped() {
        let rows = parse_emoji_tsv("a\t😀\n\n# comment\nbadtwo\nc\t\t\nd\t🙂\n");
        assert_eq!(rows, vec![("a".to_string(), "😀".to_string()), ("d".to_string(), "🙂".to_string())]);
    }

    #[test]
    fn pinyin_and_hanzi_keywords() {
        let fam = EmojiFamily::new();
        assert!(fam.predict("weixiao").iter().any(|c| c.text == "😊"), "pinyin key");
        assert!(fam.predict("微笑").iter().any(|c| c.text == "😊"), "hanzi key");
    }

    #[test]
    fn exact_beats_prefix_in_ranking() {
        let fam = EmojiFamily::new();
        // 长关键词 exact:smile → 😊 1.0。
        let cands = fam.predict("smile");
        assert_eq!(cands[0].text, "😊", "{cands:?}");
        assert_eq!(cands[0].raw_score, 1.0);
    }

    #[test]
    fn two_letter_keyword_demoted_even_on_exact() {
        // ≤2 字母关键词(ok→👍、cd→📀)即使完整命中也降为前缀档 0.6:
        // 两字母输入几乎总是中文简拼(承担/程度/成都)或常用英文缩写,
        // emoji 不该压过它们。
        let fam = EmojiFamily::new();
        let cands = fam.predict("ok");
        let ok = cands.iter().find(|c| c.text == "👍").unwrap();
        assert!((ok.raw_score - 0.6).abs() < 1e-9,
            "two-letter exact demoted to prefix tier: {}", ok.raw_score);
    }

    #[test]
    fn long_keyword_prefix_decays() {
        // 前缀命中长关键词:剩余超过 2 字符后按 0.85^剩余 衰减。
        // weishenm(8) → weishenme(9) 剩 1 → 0.6(不衰减);
        // weis(4) → weishenme(9) 剩 5 → 0.6 × 0.85³ ≈ 0.368。
        let fam = EmojiFamily::new();
        let path = std::env::temp_dir().join(format!("emoji_decay_{}.tsv", std::process::id()));
        std::fs::write(&path, "weishenme\t🤌\n").unwrap();
        fam.load_tsv(&path.to_string_lossy()).unwrap();

        let near = fam.predict("weishenm");
        let e = near.iter().find(|c| c.text == "🤌").unwrap();
        assert!((e.raw_score - 0.6).abs() < 1e-9, "剩 1 不衰减: {}", e.raw_score);

        let far = fam.predict("weis");
        let e2 = far.iter().find(|c| c.text == "🤌").unwrap();
        assert!(e2.raw_score < 0.45, "剩 5 衰减后明显更低: {}", e2.raw_score);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_match_returns_empty() {
        let fam = EmojiFamily::new();
        assert!(fam.predict("xyzzy").is_empty());
        assert!(fam.predict("").is_empty());
    }

    #[test]
    fn empty_input_returns_empty() {
        let fam = EmojiFamily::new();
        assert!(fam.predict("").is_empty());
    }

    /// Unique per-test temp dict — tests run in parallel threads (same lesson
    /// as english.rs: a shared temp path made writers race).
    fn temp_emoji_tsv(tag: &str, content: &str) -> String {
        let path = std::env::temp_dir().join(format!("emoji_test_{}_{tag}.tsv", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn tsv_load_adds_pinyin_keywords() {
        // The generated emoji.tsv carries PINYIN keywords (Chinese CLDR words
        // are converted by fetch_emoji.sh — hanzi can never be typed into the
        // pinyin buffer). Loading such a table must make them triggerable.
        let fam = EmojiFamily::new();
        let path = temp_emoji_tsv("ganlan", "ganlan\t🥦\nweixiao\t☺\n");
        assert_eq!(fam.load_tsv(&path).unwrap(), 2);
        assert!(fam.predict("ganlan").iter().any(|c| c.text == "🥦"), "ganlan triggers 🥦");
        assert!(fam.predict("weixiao").iter().any(|c| c.text == "☺"), "weixiao triggers ☺");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn later_load_overrides_keyword_keeping_position() {
        // External dict then user dict: the user's emoji wins for the same
        // keyword, and the entry stays single (position kept, no duplicates).
        let fam = EmojiFamily::new();
        let ext = temp_emoji_tsv("ext", "ganlan\t🥦\n");
        let usr = temp_emoji_tsv("usr", "ganlan\t🥬\n");
        fam.load_tsv(&ext).unwrap();
        fam.load_tsv(&usr).unwrap();
        let cands = fam.predict("ganlan");
        assert_eq!(cands[0].text, "🥬", "user dict wins: {cands:?}");
        assert_eq!(cands.len(), 1, "no duplicate entries: {cands:?}");
        let _ = std::fs::remove_file(&ext);
        let _ = std::fs::remove_file(&usr);
    }
}
