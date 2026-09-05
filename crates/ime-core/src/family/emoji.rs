//! EmojiFamily — emoji prediction as a first-class family in the unified scorer.
//!
//! Ranks alongside pinyin + english (中英混输 competition): every emoji has
//! trigger keywords — English name, pinyin, or hanzi — so `smile`, `weixiao`,
//! or `微笑` all surface 😊 as a candidate.
//!
//! ## Dictionary format (v2, the ONLY supported format)
//!
//! ```text
//! # @type: emoji-freq
//! emoji<TAB or space>freq<TAB>kw1[<TAB>kw2[<TAB>kw3]]
//! 😍	4543	hearteye	chimi	huachi
//! ```
//!
//! One line per emoji (emoji is the PRIMARY key), carrying its popularity
//! freq (1..=10000) and 1..=N trigger keywords; fields are split on any
//! whitespace. Produced by the cleaning pipeline: `clean_emoji_llm.py` →
//! `merge_emoji_clean.py` → `refine_emoji_llm.py` (LLM-assisted, local
//! Qwen). The legacy `keyword<TAB>emoji` format is NOT supported.
//!
//! ## Dictionary layers (like EnglishFamily)
//! 1. **external** — `assets/dict/emoji/emoji.tsv`, loaded at startup;
//! 2. **user** — the user's own table (`emoji_user.tsv`, same v2 format).
//!
//! Later loads override earlier ones per EMOJI (user wins); entries are
//! kept sorted by freq DESC so family top_n truncation favours popular
//! emojis.
//!
//! ## Scoring (replaces the old uniform 1.0/0.6 hardcodes)
//!
//! ```text
//! band(freq): ≥9000→0.90 / ≥7000→0.70 / ≥5000→0.50 / ≥3000→0.35 / else 0.25
//! exact  = 0.88 + 0.08 × band                    (short kw ≤2 chars → prefix rule)
//! prefix = 0.6 × prefix_decay × (0.55 + 0.45 × band)
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use super::{CandidateFamily, InputContext, ScoredCandidate};

/// One emoji, its popularity freq, and its trigger keywords (1..=N; all
/// prefix-matched against the input; duplicate keywords across entries are
/// fine — the first hit per emoji wins).
#[derive(Clone, Debug)]
struct EmojiEntry {
    emoji: String,
    freq: u32,
    keys: Vec<String>,
}

/// 流行度 5 档,阈值与 `english::frequency_band` 一致(weight-scoring.md)。
fn emoji_band(freq: u32) -> f64 {
    match freq {
        f if f >= 9000 => 0.90,
        f if f >= 7000 => 0.70,
        f if f >= 5000 => 0.50,
        f if f >= 3000 => 0.35,
        _ => 0.25,
    }
}

pub struct EmojiFamily {
    enabled: AtomicBool,
    /// 打分参数(yaml `weights.emoji` 可覆盖,见 config.rs EmojiWeightConfig)。
    weights: std::sync::Mutex<EmojiWeights>,
    /// freq 降序(每次 merge 后重排)—— top_n 截断天然偏向高频 emoji。
    entries: Mutex<Vec<EmojiEntry>>,
}

/// emoji 家族打分参数(v2)。默认值 = 引入 v2 时的实测锚点。
#[derive(Debug, Clone, Copy)]
pub struct EmojiWeights {
    /// exact 地板:exact = exact + exact_quality × band(freq)。
    pub exact: f64,
    pub exact_quality: f64,
    /// prefix 地板:prefix = prefix_base × decay × (0.55 + prefix_band_mix × band)。
    pub prefix_base: f64,
    pub prefix_band_mix: f64,
    /// ≤2 字母触发词即使完整命中也乘此折扣(两字母输入几乎总是中文简拼,
    /// emoji 让位;调高 → 两字母表情包更容易出现)。
    pub short_kw_penalty: f64,
}

impl Default for EmojiWeights {
    fn default() -> Self {
        EmojiWeights {
            exact: 0.88,
            exact_quality: 0.08,
            prefix_base: 0.6,
            prefix_band_mix: 0.45,
            short_kw_penalty: 0.6,
        }
    }
}

impl EmojiFamily {
    pub fn new() -> Self {
        EmojiFamily {
            enabled: AtomicBool::new(true),
            weights: std::sync::Mutex::new(EmojiWeights::default()),
            entries: Mutex::new(Vec::new()),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// yaml `weights.emoji` 覆盖打分参数(build_engine / fcitx5 create 调用)。
    pub fn set_weights(&self, w: EmojiWeights) {
        *self.weights.lock().unwrap() = w;
    }

    /// Merge loaded entries: a later emoji replaces an earlier one entirely
    /// (user wins over external); then re-sort freq DESC.
    fn merge(&self, rows: Vec<EmojiEntry>) -> usize {
        let mut entries = self.entries.lock().unwrap();
        let mut count = 0;
        for row in rows {
            match entries.iter_mut().find(|e| e.emoji == row.emoji) {
                Some(slot) => *slot = row,
                None => {
                    entries.push(row);
                }
            }
            count += 1;
        }
        entries.sort_by(|a, b| b.freq.cmp(&a.freq));
        count
    }

    /// Load a v2 table: `emoji freq kw...` (whitespace-separated), `#`
    /// comments and blank lines skipped. Shared by the external dict and the
    /// user dict — call order decides precedence (later wins).
    pub fn load_tsv(&self, path: &str) -> std::io::Result<usize> {
        let data = std::fs::read_to_string(path)?;
        let rows = parse_emoji_tsv(&data);
        let n = self.merge(rows);
        if n > 0 {
            tracing::info!(count = n, path, "emoji: loaded freq table");
        }
        Ok(n)
    }
}

impl Default for EmojiFamily {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse v2 lines: `emoji freq kw1 [kw2 ...]`, whitespace-separated; `#`
/// comments and blank lines skipped. A line needs ≥3 fields (emoji, freq,
/// ≥1 keyword) — anything else (incl. legacy two-column rows) is dropped.
fn parse_emoji_tsv(data: &str) -> Vec<EmojiEntry> {
    data.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let emoji = it.next()?.trim().to_string();
            let freq: u32 = it.next()?.trim().parse().ok()?;
            let freq = freq.clamp(1, 10_000);
            let keys: Vec<String> = it
                .map(|k| k.trim().to_lowercase())
                .filter(|k| k.chars().count() >= 2)
                .take(8)
                .collect();
            if emoji.is_empty() || keys.is_empty() {
                None
            } else {
                Some(EmojiEntry { emoji, freq, keys })
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
        self.enabled.load(Ordering::Relaxed)
    }

    /// 运行时开关(swift-ime.yaml → dicts.emoji: false 全家族禁用)。
    fn set_family_enabled(&self, on: bool) {
        self.set_enabled(on);
    }

    fn top_n(&self) -> usize {
        // round10 放宽 4 → 8;可经 `weights.family_top_n.emoji` 覆盖。
        8
    }

    fn predict(&self, input: &str, _ctx: &InputContext) -> Vec<ScoredCandidate> {
        // Single-char input is skipped: with a large keyword table, "a"/"h" would
        // match hundreds of generic keywords and flood the candidate list.
        if input.chars().count() < 2 {
            return Vec::new();
        }
        let entries = self.entries.lock().unwrap();
        let mut out = Vec::new();
        let input_len = input.chars().count();
        for entry in entries.iter() {
            for kw in &entry.keys {
                if kw.starts_with(input) {
                    let exact = kw == input;
                    let band = emoji_band(entry.freq);
                    // 前缀分 0.6 地板 + 距离衰减:用户打中文途中(weishenm →
                    // weishenme→🤌)emoji 只前缀命中时,应明显低于拼音家族的
                    // 联想候选(0.5+),不插队。剩余 ≤2 字符视为"马上打完"
                    // 不衰减(与拼音前缀联想同规则,免费额度 3),超出按
                    // 0.85^超出 衰减。
                    //
                    // 流行度调制(P0 清洗 §2.4):(0.55+0.45×band) 让顶流
                    // emoji 前缀靠前、长尾沉底 —— 替换旧版全家族统一 0.6。
                    //
                    // ≤2 字母的短关键词(cd→📀、ok→👍)即使完整命中也降为
                    // 前缀档:两字母输入几乎总是中文简拼(承担/程度/成都)或
                    // 常用英文缩写,emoji 不该压过它们。
                    let short_kw = kw.chars().count() <= 2;
                    let w = *self.weights.lock().unwrap();
                    let exact_score = w.exact + w.exact_quality * band;
                    let score = if exact {
                        if short_kw {
                            // ≤2 字母触发词折扣(可配置;默认 0.6 —— emoji
                            // 让位中文简拼但仍可见)。
                            exact_score.min(1.0) * w.short_kw_penalty
                        } else {
                            exact_score.min(1.0)
                        }
                    } else {
                        let diff = kw.chars().count().saturating_sub(input_len);
                        w.prefix_base
                            * crate::family::scoring::prefix_decay(diff)
                            * (0.55 + w.prefix_band_mix * band)
                    };
                    out.push(ScoredCandidate {
                        text: entry.emoji.clone(),
                        family: "emoji",
                        source: if exact { "exact" } else { "prefix" },
                        raw_score: score,
                    });
                    break; // 同 emoji 的首个命中 kw 即可(keys 按"最可能打出"排序)
                }
            }
        }
        drop(entries);
        out.sort_by(|a, b| {
            b.raw_score
                .partial_cmp(&a.raw_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        self.load_tsv(path)
    }

    fn load_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.load_tsv(path)
    }
}

/// Arc 共享句柄的 trait 委托(D5):engine 持 `Arc<EmojiFamily>` 直调
/// 家族私有方法(set_weights),scorer 持同一 Arc 当 trait 参与统一排序。
impl CandidateFamily for std::sync::Arc<EmojiFamily> {
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn priority(&self) -> u32 {
        (**self).priority()
    }
    fn enabled(&self) -> bool {
        (**self).enabled()
    }
    fn set_family_enabled(&self, on: bool) {
        (**self).set_family_enabled(on)
    }
    fn top_n(&self) -> usize {
        (**self).top_n()
    }
    fn predict(&self, input: &str, ctx: &InputContext) -> Vec<ScoredCandidate> {
        (**self).predict(input, ctx)
    }
    fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        (**self).load_dict(path)
    }
    fn load_user_dict(&self, path: &str) -> std::io::Result<usize> {
        (**self).load_user_dict(path)
    }
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per-test temp dict — tests run in parallel threads (same lesson
    /// as english.rs: a shared temp path made writers race).
    fn temp_v2(tag: &str, content: &str) -> String {
        let path =
            std::env::temp_dir().join(format!("emoji_v2_{}_{tag}.tsv", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn exact_keyword_scores_with_band() {
        // v2 exact = 0.88 + 0.08 × band(freq):9000 → band 0.90 → 0.952。
        // 替换旧版 1.0 硬顶 —— emoji exact 不再与中文 single 同价。
        let fam = EmojiFamily::new();
        let path = temp_v2("exact", "\u{1f60a} 9000 smile weixiao");
        fam.load_tsv(&path).unwrap();
        let cands = fam.predict("smile", &InputContext::new());
        assert_eq!(cands[0].text, "\u{1f60a}");
        assert!((cands[0].raw_score - 0.952).abs() < 1e-9, "{:?}", cands[0].raw_score);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prefix_modulated_by_band() {
        // v2 prefix = 0.6 × decay × (0.55 + 0.45 × band):
        // smil→smile 剩 1(≤2 免衰减)→ 0.6 × (0.55+0.45×0.90) = 0.573。
        let fam = EmojiFamily::new();
        let path = temp_v2("prefix", "\u{1f60a} 9000 smile");
        fam.load_tsv(&path).unwrap();
        let cands = fam.predict("smil", &InputContext::new());
        let e = cands.iter().find(|c| c.text == "\u{1f60a}").unwrap();
        assert!((e.raw_score - 0.573).abs() < 1e-9, "prefix band-modulated: {}", e.raw_score);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn low_freq_prefix_sinks() {
        // 长尾(band 0.25)prefix = 0.6 × 0.6625 ≈ 0.3975,低于高频的 0.573
        // —— "要么见不到要么压文字" 的振荡由 band 调制消除。
        let fam = EmojiFamily::new();
        let hi = temp_v2("hi", "\u{1f60a} 9000 smile");
        let lo = temp_v2("lo", "\u{1f9d0} 100 nerd smilerk");
        fam.load_tsv(&hi).unwrap();
        fam.load_tsv(&lo).unwrap();
        let cands = fam.predict("smil", &InputContext::new());
        let hi_score = cands.iter().find(|c| c.text == "\u{1f60a}").unwrap().raw_score;
        let lo_score = cands.iter().find(|c| c.text == "\u{1f9d0}").unwrap().raw_score;
        assert!(hi_score > lo_score, "high freq prefix outranks low: {cands:?}");
        assert!((lo_score - 0.6 * 0.6625).abs() < 1e-9, "{}", lo_score);
        let _ = std::fs::remove_file(&hi);
        let _ = std::fs::remove_file(&lo);
    }

    #[test]
    fn short_keyword_demoted_even_on_exact() {
        // ≤2 字母关键词(ok→👍)即使完整命中也乘 short_kw_penalty:
        // exact 0.952 × 0.6 = 0.5712 —— 两字母输入几乎总是中文简拼
        // (承担/程度/成都),emoji 让位但仍可见(可配,调高即更容易冒头)。
        let fam = EmojiFamily::new();
        let path = temp_v2("short", "\\u{1f44d} 9500 ok hao");
        fam.load_tsv(&path).unwrap();
        let cands = fam.predict("ok", &InputContext::new());
        let ok = cands.iter().find(|c| c.text == "\\u{1f44d}").unwrap();
        assert!((ok.raw_score - 0.952 * 0.6).abs() < 1e-9, "{}", ok.raw_score);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn long_keyword_prefix_decays() {
        // 前缀命中长关键词:weishenm(8)→weishenme(9) 剩 1 不衰减;
        // weis(4)→ 剩 5 → 0.85² 衰减。freq 500 → band 0.25 → 调制 0.6625。
        let fam = EmojiFamily::new();
        let path = temp_v2("decay", "\u{1f90c} 500 weishenme");
        fam.load_tsv(&path).unwrap();
        let near = fam.predict("weishenm", &InputContext::new());
        let e = near.iter().find(|c| c.text == "\u{1f90c}").unwrap();
        assert!((e.raw_score - 0.6 * 0.6625).abs() < 1e-9, "剩 1 不衰减: {}", e.raw_score);
        let far = fam.predict("weis", &InputContext::new());
        let e2 = far.iter().find(|c| c.text == "\u{1f90c}").unwrap();
        assert!(e2.raw_score < 0.6 * 0.6625 * 0.8, "剩 5 衰减后明显更低: {}", e2.raw_score);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn single_char_input_skipped() {
        let fam = EmojiFamily::new();
        let path = temp_v2("one", "\u{1f60a} 9000 smile ha");
        fam.load_tsv(&path).unwrap();
        assert!(fam.predict("a", &InputContext::new()).is_empty());
        assert!(fam.predict("微", &InputContext::new()).is_empty(), "even hanzi needs 2 chars");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_match_returns_empty() {
        let fam = EmojiFamily::new();
        assert!(fam.predict("xyzzy", &InputContext::new()).is_empty());
        assert!(fam.predict("", &InputContext::new()).is_empty());
    }

    #[test]
    fn whitespace_and_tab_both_parse() {
        // 设计要求:空白字符分割,不限定 tab。
        let rows = parse_emoji_tsv(
            "\u{1f60a}\t9000\tsmile\tweixiao\n\u{1f44d} 9500 zan hao\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].emoji, "\u{1f60a}");
        assert_eq!(rows[0].freq, 9000);
        assert_eq!(rows[0].keys, vec!["smile", "weixiao"]);
        assert_eq!(rows[1].keys, vec!["zan", "hao"]);
    }

    #[test]
    fn malformed_lines_skipped() {
        // 两列(旧格式)行、freq 非数、kw 全 1 字符的行都丢弃;freq 越界 clamp。
        let rows = parse_emoji_tsv(
            "smile\t\u{1f60a}\n\u{1f60a}\tabc\tsmile\n\u{1f44d}\t9500\tok\na\t1\tb\n\u{1f60b}\t99999\tyum\n",
        );
        assert_eq!(rows.len(), 2, "valid rows survive, legacy dropped: {rows:?}");
        assert_eq!(rows[0].emoji, "\u{1f44d}");
        assert_eq!(rows[0].keys, vec!["ok"]);
        assert_eq!(rows[1].emoji, "\u{1f60b}");
        assert_eq!(rows[1].freq, 10000, "freq clamped to 10000");
    }

    #[test]
    fn later_load_overrides_earlier_per_emoji() {
        // user 覆盖 external:同 emoji 主键后加载的整行生效。
        let fam = EmojiFamily::new();
        let ext = temp_v2("ext", "\u{1f60a} 9000 smile weixiao");
        let user = temp_v2("user", "\u{1f60a} 10000 happy laugh");
        fam.load_tsv(&ext).unwrap();
        fam.load_tsv(&user).unwrap();
        let cands = fam.predict("smile", &InputContext::new());
        assert!(
            !cands.iter().any(|c| c.text == "\u{1f60a}"),
            "external kw replaced: {cands:?}"
        );
        let cands = fam.predict("laugh", &InputContext::new());
        assert_eq!(cands[0].text, "\u{1f60a}", "user kw wins");
        assert!((cands[0].raw_score - 0.952).abs() < 1e-9, "user freq top band: {}", cands[0].raw_score);
        let _ = std::fs::remove_file(&ext);
        let _ = std::fs::remove_file(&user);
    }

    #[test]
    fn hanzi_and_pinyin_keywords_trigger() {
        let fam = EmojiFamily::new();
        let path = temp_v2("hanzi", "\u{1f60a} 9000 weixiao \u{5fae}\u{7b11}");
        fam.load_tsv(&path).unwrap();
        assert!(
            fam.predict("weixiao", &InputContext::new()).iter().any(|c| c.text == "\u{1f60a}"),
            "pinyin key"
        );
        assert!(
            fam.predict("微笑", &InputContext::new()).iter().any(|c| c.text == "\u{1f60a}"),
            "hanzi key"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn merge_keeps_single_entry_per_emoji() {
        let fam = EmojiFamily::new();
        let ext = temp_v2("m1", "\u{1f957} 5000 salad gaoxing");
        let usr = temp_v2("m2", "\u{1f957} 6000 salad gaoxing");
        fam.load_tsv(&ext).unwrap();
        fam.load_tsv(&usr).unwrap();
        let cands = fam.predict("salad", &InputContext::new());
        assert_eq!(cands.len(), 1, "no duplicate entries: {cands:?}");
        assert_eq!(cands[0].text, "\u{1f957}");
        assert!((cands[0].raw_score - (0.88 + 0.08 * 0.50)).abs() < 1e-9);
        let _ = std::fs::remove_file(&ext);
        let _ = std::fs::remove_file(&usr);
    }
}
