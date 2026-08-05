//! EmojiFamily — emoji prediction as a first-class family in the unified scorer.
//!
//! Ranks alongside pinyin + english (中英混输 competition): every emoji has
//! THREE keyword groups — English name, pinyin, and hanzi — so `smile`,
//! `weixiao`, or `微` all surface 😊 as a candidate. Prefix match shows the
//! emoji while the user is still typing; exact match scores 1.0.
//!
//! No special trigger prefix: the family participates in the normal candidate
//! competition, and its priority (default 60, below english 70) keeps emoji
//! from drowning out text candidates.

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
    EmojiEntry { emoji: "😅", keys: &["sweat", "gan ga", "尴尬", "汗"] },
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
}

impl EmojiFamily {
    pub fn new() -> Self {
        EmojiFamily { enabled: true }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl Default for EmojiFamily {
    fn default() -> Self {
        Self::new()
    }
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
        if input.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for e in EMOJI_TABLE {
            let mut hit = false;
            for k in e.keys {
                if k.starts_with(input) {
                    // Exact keyword → 1.0; prefix → 0.8 (below english exact).
                    let score = if *k == input { 1.0 } else { 0.8 };
                    out.push(ScoredCandidate {
                        text: e.emoji.to_string(),
                        family: "emoji",
                        source: if *k == input { "exact" } else { "prefix" },
                        raw_score: score,
                    });
                    hit = true;
                    break; // one candidate per emoji
                }
            }
            let _ = hit;
        }
        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap_or(std::cmp::Ordering::Equal));
        out
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
        assert_eq!(cands[0].raw_score, 0.8, "prefix match scores 0.8");
    }

    #[test]
    fn pinyin_and_hanzi_keywords() {
        let fam = EmojiFamily::new();
        assert!(fam.predict("weixiao").iter().any(|c| c.text == "😊"), "pinyin key");
        assert!(fam.predict("微").iter().any(|c| c.text == "😊"), "hanzi key");
    }

    #[test]
    fn exact_beats_prefix_in_ranking() {
        let fam = EmojiFamily::new();
        // "ok" matches 👍 exact (1.0) — and nothing else exact; prefix hits stay lower.
        let cands = fam.predict("ok");
        assert_eq!(cands[0].text, "👍", "{cands:?}");
        assert_eq!(cands[0].raw_score, 1.0);
        // All remaining candidates are prefix matches (≤ 0.8).
        assert!(cands.iter().skip(1).all(|c| c.raw_score <= 0.8), "{cands:?}");
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
}
