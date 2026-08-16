//! PinyinFamily — Chinese full-pinyin prediction using inputx-pinyin's
//! embedded dictionary + bigram Viterbi composition + PhraseBook recall.

use super::{CandidateFamily, InputContext, ScoredCandidate};
use self::phrase::PhraseBook;
use crate::recency::RecentStore;

pub mod dict;
pub mod engine;
pub mod lattice;
pub mod phrase;

use dict::LargeDict;
use std::sync::{Arc, Mutex};

use crate::store::WeightStore;

/// Full-pinyin prediction family.
///
/// Scoring:
/// - LargeDict exact match: `raw_score = 1.0` (900K+ entries, O(1))
/// - PhraseBook exact match: `raw_score = 0.95` (small, user-custom)
/// - Viterbi composition: normalized log-likelihood → [0.3, 0.95]
/// - Session lookup: `raw_score = 0.5`
/// - Prefix fallback: `raw_score = 0.3`
/// - PhraseBook prefix: `raw_score = 0.85`
pub struct PinyinFamily {
    engine: inputx_pinyin::PinyinEngine,
    phrase_book: Mutex<PhraseBook>,
    large_dict: Mutex<LargeDict>,
    lattice: Mutex<Option<lattice::LatticeDecoder>>,
    recency: Mutex<RecentStore>,
    enabled: bool,
    weights: PinyinWeights,
    store: Mutex<Option<Arc<WeightStore>>>,
    /// freq→score 映射参数(swift-ime.yaml → weights.freq_scale)。
    freq_scale: crate::scoring::FreqScale,
    /// 上下文感知开关(swift-ime.yaml → input.context_aware,默认开)。
    /// 关闭时 `predict_with_context` 退化为纯 `predict` —— 不做 recency /
    /// 整词联想加成,候选排序完全由词典频率决定。
    /// `Mutex<bool>` 以便 trait 的 `&self` setter 写入。
    context_aware: Mutex<bool>,
    /// 上一次提交的 (word, pinyin) —— 前缀整词联想的上下文来源。
    /// 例:提交 中(zhong)后输入 de,联想 zhong+de="zhongde" 的整词。
    last_commit: Mutex<(String, String)>,
}

/// Configurable scoring weights for the pinyin family.
/// All values are tunable via swift-ime.yaml → weights.pinyin section.
#[derive(Debug, Clone)]
pub struct PinyinWeights {
    // ── Member base scores ──
    pub phrase_book: f64,
    pub large_dict: f64,
    pub viterbi_base: f64,
    pub viterbi_scale: f64,
    pub jianpin: f64,
    /// 全拼前缀联想折扣(naozh → naozhong 闹钟)。
    pub prefix_lookup: f64,
    pub single_syl_decay: f64,
    pub context_boost: f64,
    // ── Post-merge adjustments ──
    pub stopword_penalty: f64,   // multiplier for all-stopword compositions
    pub confirm_bonus: f64,      // bonus for dict∩viterbi confirmation
    pub short_word_bonus: f64,   // bonus per 2-char word
    // ── Take limits ──
    pub large_dict_take: usize,
    pub viterbi_take: usize,
    pub jianpin_take: usize,
}

impl Default for PinyinWeights {
    fn default() -> Self {
        PinyinWeights {
            phrase_book: 0.88, large_dict: 0.85,
            viterbi_base: 0.25, viterbi_scale: 0.55,
            jianpin: 0.50, prefix_lookup: 0.75,
            single_syl_decay: 0.5, context_boost: 0.12,
            stopword_penalty: 0.5, confirm_bonus: 0.05, short_word_bonus: 0.01,
            large_dict_take: 96, viterbi_take: 48,
            jianpin_take: 8,
        }
    }
}

impl PinyinFamily {
    pub fn new() -> Self {
        Self::with_scoring(PinyinWeights::default(), crate::scoring::ScoringConfig::default())
    }

    pub fn with_weights(weights: PinyinWeights) -> Self {
        Self::with_scoring(weights, crate::scoring::ScoringConfig::default())
    }

    /// Full construction: pinyin weights + the unified scoring config (recency
    /// boosts, bigram ceiling, freq→score scale) from `swift-ime.yaml`.
    pub fn with_scoring(
        weights: PinyinWeights,
        scoring: crate::scoring::ScoringConfig,
    ) -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(PhraseBook::default_phrases()),
            large_dict: Mutex::new(LargeDict::new()),
            lattice: Mutex::new(None),
            recency: Mutex::new(RecentStore::new()),
            enabled: true,
            weights,
            store: Mutex::new(None),
            freq_scale: scoring.freq_scale,
            context_aware: Mutex::new(true),
            last_commit: Mutex::new((String::new(), String::new())),
        }
    }

    pub fn set_weights(&mut self, w: PinyinWeights) { self.weights = w; }

    pub fn with_phrase_book(phrase_book: PhraseBook) -> Self {
        Self::with_scoring_and_phrase_book(
            PinyinWeights::default(),
            crate::scoring::ScoringConfig::default(),
            phrase_book,
        )
    }

    fn with_scoring_and_phrase_book(
        weights: PinyinWeights,
        scoring: crate::scoring::ScoringConfig,
        phrase_book: PhraseBook,
    ) -> Self {
        PinyinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            phrase_book: Mutex::new(phrase_book),
            large_dict: Mutex::new(LargeDict::new()),
            lattice: Mutex::new(None),
            recency: Mutex::new(RecentStore::new()),
            enabled: true,
            weights,
            store: Mutex::new(None),
            freq_scale: scoring.freq_scale,
            context_aware: Mutex::new(true),
            last_commit: Mutex::new((String::new(), String::new())),
        }
    }

    /// Attach the weight store for persisting learned phrases.
    pub fn set_store(&self, store: Arc<WeightStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    /// Warm the phrase book from persisted SQLite data (internal helper).
    fn do_warm_phrases(&self) {
        let guard = self.store.lock().unwrap();
        if let Some(ref store) = *guard {
            let entries = store.load_all_phrases();
            if !entries.is_empty() {
                let mut book = self.phrase_book.lock().unwrap();
                for (pinyin, word, priority, count) in &entries {
                    // 存量过滤:早期版本把 emoji 也学进了 phrase(见
                    // learn_phrase_inner),加载时丢弃。
                    if !is_learnable_word(word) {
                        continue;
                    }
                    book.insert_with_order_count(pinyin, word, *priority, *count);
                }
                eprintln!("[ime-core] pinyin: warmed {} phrases from store", entries.len());
            }
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) { self.enabled = enabled; }

    /// Record a committed word for the recent member: stamps the current
    /// wall-clock time and double-writes the table to SQLite (full-snapshot
    /// replace, ≤512 rows) so the time-decay survives restarts.
    pub fn record_commit(&self, word: &str) {
        let mut rec = self.recency.lock().unwrap();
        rec.record(word, now_ms());
        if let Some(ref store) = *self.store.lock().unwrap() {
            store.save_recency(&rec.dump());
        }
    }

    pub fn record_pick(&self, pinyin: &str, word: &str) {
        self.engine.dict().record_pick(pinyin, word);
    }

    pub fn engine(&self) -> &inputx_pinyin::PinyinEngine { &self.engine }
    pub fn phrase_count(&self) -> usize { self.phrase_book.lock().unwrap().len() }
    pub fn large_dict_len(&self) -> usize { self.large_dict.lock().unwrap().len() }

    /// 该词是否已存在于词典(inputx 嵌入大词典或 rime-ice lattice)?
    fn in_dictionary(&self, pinyin: &str, word: &str) -> bool {
        if self.engine.dict().lookup(pinyin).iter().any(|w| w == word) {
            return true;
        }
        if let Some(lat) = self.lattice.lock().unwrap().as_ref() {
            if lat.has_word(pinyin, word) {
                return true;
            }
        }
        false
    }

    /// 加入/递增单词本的公共实现(已通过词典检查或自生词无条件路径)。
    /// 只收汉字/字母数字组成的词:emoji 等符号候选(提交 cd→📀 后被学成
    /// pinyin/phrase,吃 phrase+recent 双重加成霸榜)不进拼音单词本 ——
    /// emoji 有自己的关键词表,学习走 emoji 体系。
    fn learn_phrase_inner(&self, pinyin: &str, hanzi: &str) {
        if !is_learnable_word(hanzi) {
            return;
        }
        let mut book = self.phrase_book.lock().unwrap();
        if book.count(pinyin, hanzi) > 0 {
            book.bump_count(pinyin, hanzi);
            if let Some(ref store) = *self.store.lock().unwrap() {
                store.bump_phrase_count(pinyin, hanzi);
            }
        } else {
            book.insert(pinyin, hanzi);
            // Persist to SQLite if store is attached.
            if let Some(ref store) = *self.store.lock().unwrap() {
                store.record_phrase(pinyin, hanzi, 0);
            }
        }
    }

    /// 自造词的使用次数 → 参与排名的基础分:首次 0.70(低于词典精确分
    /// 0.85+,不会压过词典词),每次使用 +0.02,封顶 phrase_book 权重
    /// (默认 0.88)—— 高频自造词随使用逐步靠前,而不是所有 phrase 词
    /// 共享一个固定高分。
    fn phrase_score(&self, count: u32) -> f64 {
        (0.70 + 0.02 * count.saturating_sub(1) as f64)
            .min(self.weights.phrase_book)
    }
}

/// Extract initials from a raw (concatenated) pinyin string.
/// "shengnengshengqiao" → "snsq", "nihao" → "nh".
pub fn initials_from_pinyin(raw: &str) -> String {
    let segs = inputx_pinyin::segment(raw);
    segs.first()
        .map(|seg| seg.syllables.iter().filter_map(|s| s.chars().next()).collect())
        .unwrap_or_default()
}

impl Default for PinyinFamily {
    fn default() -> Self { Self::new() }
}

/// 单词本只收**含汉字**的词(纯汉字或汉字+ASCII 混合,如 "Bevy引擎");
/// 纯 ASCII 英文词(name/cd)走英文自生词体系(en_user),emoji / 符号
/// 一律不学 —— 都不是拼音自造词。
fn is_learnable_word(word: &str) -> bool {
    word.chars().any(is_cjk)
        && word.chars().all(|c| is_cjk(c) || c.is_ascii_alphanumeric())
}

/// CJK 统一表意文字(含扩展A)。
fn is_cjk(c: char) -> bool {
    let p = c as u32;
    (0x4E00..=0x9FFF).contains(&p) || (0x3400..=0x4DBF).contains(&p)
}

/// 当前 wall-clock 毫秒(unix epoch)—— recent member 的时间基准。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl CandidateFamily for PinyinFamily {
    fn name(&self) -> &'static str { "pinyin" }
    fn priority(&self) -> u32 { 100 }
    fn enabled(&self) -> bool { self.enabled }
    fn top_n(&self) -> usize { 128 }

    fn record_pick(&self, pinyin: &str, word: &str) {
        self.engine.dict().record_pick(pinyin, word);
        // 前缀整词联想的上下文:记录本次提交的 (word, pinyin)。
        *self.last_commit.lock().unwrap() = (word.to_string(), pinyin.to_string());
        // 注意:此处**不**调 learn_phrase —— 单词本的唯一入口是造词路径
        // (learn_composed_phrase,见 state.rs select)。record_pick 在逐字
        // 选择时对**单字**也会调用,学短语会把"李"这类单字塞进单词本。
        // Persist the L0 user model (pins + pick counters) — same double-write
        // cadence as recency, so the 3-pick auto-pin survives restarts.
        if let Some(ref store) = *self.store.lock().unwrap() {
            store.save_l0(&self.export_l0_json());
        }
    }

    fn learn_phrase(&self, pinyin: &str, hanzi: &str) {
        // 已在词库(输入x 大词典 / rime-ice)的词不加入单词本 —— phrase 是给
        // 自造词的(de→的 不该被记入);已学的自造词再次选中只增加使用次数,
        // 分数随使用频率上升参与排名。
        if self.in_dictionary(pinyin, hanzi) {
            return;
        }
        self.learn_phrase_inner(pinyin, hanzi);
    }

    fn learn_composed_phrase(&self, pinyin: &str, hanzi: &str) {
        // 自生词流程:用户输入多字拼音后通过数字键逐字选择组成的整体,
        // 无条件加入单词本(主动造词,不因词典里恰好有该词而跳过)。
        self.learn_phrase_inner(pinyin, hanzi);
    }

    fn export_l0_json(&self) -> String {
        let snap = self.engine.dict().export_l0();
        let mut json = String::from("{\"pins\":[");
        for (i, (py, w)) in snap.pins.iter().enumerate() {
            if i > 0 { json.push(','); }
            json.push_str(&format!("[\"{py}\",\"{w}\"]"));
        }
        json.push_str("],\"picks\":[");
        let mut first = true;
        for (py, w, c) in &snap.pick_counts {
            if !first { json.push(','); } first = false;
            json.push_str(&format!("[\"{py}\",\"{w}\",{c}]"));
        }
        json.push_str("]}");
        json
    }

    fn import_l0_json(&self, json: &str) -> usize {
        #[derive(serde::Deserialize)]
        struct L0Json { pins: Vec<(String, String)>, #[serde(default)] picks: Vec<(String, String, u32)> }
        if let Ok(data) = serde_json::from_str::<L0Json>(json) {
            let snap = inputx_pinyin::L0Snapshot { pins: data.pins, pick_counts: data.picks };
            self.engine.dict().import_l0(snap)
        } else { 0 }
    }

    fn warm_recencies(&self, entries: Vec<(String, i64)>) {
        if !entries.is_empty() {
            let count = entries.len();
            // 加载时丢弃超过 3d 窗口的过期条目(RecentStore::load_bulk)。
            self.recency.lock().unwrap().load_bulk(entries, now_ms());
            eprintln!("[ime-core] pinyin: warmed {count} recency entries from store");
        }
    }

    fn record_commit(&self, word: &str) {
        // Delegate to the inherent impl (which pushes + persists the ring).
        PinyinFamily::record_commit(self, word);
    }

    fn set_context_aware(&self, on: bool) {
        *self.context_aware.lock().unwrap() = on;
    }

    fn attach_store(&self, store: std::sync::Arc<WeightStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    fn warm_phrases_from_store(&self) {
        self.do_warm_phrases();
    }

    fn load_dict_bytes(&self, data: &[u8]) -> usize {
        if data.len() > 100_000 {
            let n = self.large_dict.lock().unwrap().load_from_tsv_bytes(data);
            // After loading, try to build lattice from the FST.
            // LargeDict's backend stores the FST; we can't access it directly.
            // For now, lattice is built when loading from file (load_fst_file).
            n
        } else {
            self.phrase_book.lock().unwrap().load_from_tsv_bytes(data)
        }
    }

    fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        if path.ends_with(".json") {
            let json = std::fs::read_to_string(path)?;
            let mut book = self.phrase_book.lock().unwrap();
            book.load_from_json_str(&json)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        } else if path.ends_with(".fst") {
            // FST: load and build LatticeDecoder, passing path for .idx cache.
            let data = std::fs::read(path)?;
            let dict = inputx_fsa::Dict::new(data)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}")))?;
            *self.lattice.lock().unwrap() = Some(lattice::LatticeDecoder::new(dict, path));
            Ok(0) // size not tracked for FST
        } else {
            let meta = std::fs::metadata(path)?;
            if meta.len() > 100_000 {
                self.large_dict.lock().unwrap().load_from_tsv_file(path)
            } else {
                self.phrase_book.lock().unwrap().load_from_tsv(path)
            }
        }
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() { return Vec::new(); }

        let dict = self.engine.dict();
        let mut out = Vec::new();

        let is_single_syllable = inputx_pinyin::is_valid_syllable(input);

        // ── Primary: single-syllable lookup or unified lattice ──
        if is_single_syllable {
            let words = dict.lookup(input);
            let total = words.len().max(1) as f64;
            for (i, word) in words.into_iter().enumerate() {
                out.push(ScoredCandidate {
                    text: word, family: "pinyin", source: "single",
                    raw_score: (self.weights.large_dict - (i as f64 / total) * self.weights.single_syl_decay).clamp(0.0, 1.0),
                });
            }
        } else {
            // Unified lattice: handles full pinyin, jianpin, mixed in one pass.
            // Full pinyin matches keep full freq_to_score; mixed/initials
            // (简拼/混写) are discounted by the jianpin weight so they
            // don't drown out English exact matches for ambiguous inputs.
            let lattice_guard = self.lattice.lock().unwrap();
            if let Some(ref lat) = *lattice_guard {
                let results = lat.predict(input, self.weights.large_dict_take);
                for r in results {
                    let base_score = lat.freq_to_score(&self.freq_scale, r.freq_score as u64);
                    let (source, score) = match r.match_type {
                        lattice::MatchType::Full => ("lattice", base_score),
                        lattice::MatchType::Mixed => ("lattice_mix", base_score * self.weights.jianpin),
                        lattice::MatchType::Initials => ("lattice_jp", base_score * self.weights.jianpin),
                        // predict() 不产 Prefix(前缀联想走单独的 predict_prefix
                        // 合并分支);此处不可达,防御性兜底。
                        lattice::MatchType::Prefix => ("lattice_prefix", base_score * self.weights.prefix_lookup),
                    };
                    out.push(ScoredCandidate {
                        text: r.text, family: "pinyin", source,
                        raw_score: score,
                    });
                }
            }
            // ── 全拼前缀联想:输入是词条拼音的前缀(naozh → naozhong 闹钟)──
            // 覆盖 greedy_parse 切不开的半截音节(zh 非法 → 拆 z+h 两段,
            // pattern_match 段数对不上,旧路径永远联想不出目标词)。
            // 权重 = 词频权重 × prefix_lookup(低于全拼精确,高于 emoji/简拼)。
            if let Some(lat) = lattice_guard.as_ref() {
                for r in lat.predict_prefix(input, 16) {
                    // 同文本候选取高分(先到的 mix/简拼版本若分更低,前缀
                    // 联想版本提升之 —— 旧的"已存在则跳过"让低分 mix 挡掉
                    // 高分前缀联想,继续 0.45 挡 0.675,机械的 0.59 反超 #1)。
                    let prefix_score = {
                        let base_score = lat.freq_to_score(&self.freq_scale, r.freq_score as u64);
                        // 距离衰减:联想词拼音比输入长越多,越不可信。剩余
                        // ≤3 字符视为"马上打完"不衰减 —— 覆盖"半截声母到
                        // 完整音节"的典型差(zh→zhong 差 3):naozh 到 naozhong
                        // 与到 naozhe 同权,拼词频,高频的闹钟胜;超出部分按
                        // 0.85^超出 衰减 —— jix→jixiaokao(差 6,超出 3)这类
                        // 宽前缀捞到的高频长词沉底,不淹没目标短词。
                        let diff = r.pinyin.chars().count()
                            .saturating_sub(input.chars().count());
                        let decay = crate::scoring::prefix_decay(diff);
                        base_score * self.weights.prefix_lookup * decay
                    };
                    match out.iter_mut().find(|c| c.text == r.text) {
                        Some(existing) if prefix_score > existing.raw_score => {
                            existing.raw_score = prefix_score;
                            existing.source = "lattice_prefix";
                        }
                        Some(_) => {}
                        None => out.push(ScoredCandidate {
                            text: r.text, family: "pinyin", source: "lattice_prefix",
                            raw_score: prefix_score,
                        }),
                    }
                }
            }
            drop(lattice_guard);

            // Viterbi decomposition — always runs as fallback (造词).
            let comps = dict.top_k_compositions(input, self.weights.viterbi_take);
            for (_s, word) in comps.iter().take(16) {
                if !out.iter().any(|c| c.text == *word) {
                    out.push(ScoredCandidate {
                        text: word.clone(), family: "pinyin", source: "decomp",
                        raw_score: 0.4,
                    });
                }
            }
        }

        // ── PhraseBook: user phrases promote new words, never downgrade dict hits ──
        // A learned word that ALSO exists in the dictionary keeps its dict score
        // when that's higher (previously the phrase entry REPLACED it at the
        // fixed 0.88 — e.g. 继续's full-pinyin hit dropped below 急须). Only when
        // the dict hit scores LOWER (rare/low-frequency word the user favors)
        // does the phrase entry take over.
        {
            let book = self.phrase_book.lock().unwrap();
            for w in book.exact(input) {
                // 使用次数驱动的 phrase 分(首次 0.70,随使用升到 phrase_book)。
                let score = self.phrase_score(book.count(input, &w));
                let dict_score = out.iter().find(|c| c.text == w).map(|c| c.raw_score);
                match dict_score {
                    Some(s) if s >= score => {
                        // Dict hit already scores higher — keep it (do not
                        // retain-remove + re-add at a lower score).
                    }
                    _ => {
                        out.retain(|c| c.text != w);
                        out.push(ScoredCandidate { text: w, family: "pinyin", source: "phrase", raw_score: score });
                    }
                }
            }
            // ── PhraseBook initials match (lzm → 李正明) ──
            for w in book.by_initials(input) {
                if !out.iter().any(|c| c.text == w) {
                    out.push(ScoredCandidate {
                        text: w.clone(), family: "pinyin", source: "phrase_sp",
                        raw_score: self.phrase_score(book.count(input, &w)) * 0.95,
                    });
                }
            }
        }

        // ── Short-word bonus ──
        for c in &mut out {
            if c.text.chars().count() == 2 {
                c.raw_score = (c.raw_score + self.weights.short_word_bonus).min(1.0);
            }
        }

        if !out.is_empty() {
            out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
            return out;
        }

        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out
    }

    fn predict_with_context(&self, input: &str, _ctx: &InputContext) -> Vec<ScoredCandidate> {
        // 上下文感知暂时关闭(input.context_aware: false):候选排序完全由
        // 词典频率决定,不做 recency / 整词联想加成。
        if !*self.context_aware.lock().unwrap() {
            return self.predict(input);
        }
        let mut candidates = self.predict(input);
        if candidates.is_empty() { return candidates; }

        // ── Layer 1: Recent member boost (近期指数 → 权重合成) ──
        // b = 近期指数(1-5,按距上次使用时间分档;>3d 条目在查询时被移出)。
        // 合成公式:z = (1-a)(a+b)/8 + a —— 增量与 (1-a) 成比例,低权重词
        // 获得更大加成,高权重词增量趋零,z 天然 < 1(不会顶满 1.0)。
        let mut recency = self.recency.lock().unwrap();
        if !recency.is_empty() {
            let now = now_ms();
            for c in &mut candidates {
                let b = recency.tier(&c.text, now);
                if b > 0 {
                    let a = c.raw_score;
                    c.raw_score = (1.0 - a) * (a + b as f64) / 8.0 + a;
                }
            }
        }
        drop(recency);

        // ── Layer 2: 前缀整词联想(替换旧的 bigram/surrounding/字符级 boost)──
        // 上一提交词的拼音 + 当前输入拼音 → 查词典整词;整词以上一词开头的,
        // 剩余尾字作为候选,权重 = 整词的词频权重。
        // 例:提交 中(zhong)后输入 de → "zhongde" → 中的(9307)→ 尾字"的"以
        // "中的"的权重出现;"shide" → 是的(350380)→ "的" 权重极高。
        // 权重来自整词频率,不顶满 1.0,也不做加法/乘法噪声。
        let last = self.last_commit.lock().unwrap();
        if !last.0.is_empty() && !last.1.is_empty() {
            let joined = format!("{}{}", last.1, input);
            if let Some(lat) = self.lattice.lock().unwrap().as_ref() {
                for (word, freq) in lat.words_for(&joined) {
                    if let Some(tail) = word.strip_prefix(last.0.as_str()) {
                        if !tail.is_empty() {
                            let score = lat.freq_to_score(&self.freq_scale, freq);
                            match candidates.iter_mut().find(|c| c.text == tail) {
                                // 尾字已在候选(几乎总是):整词权重更高则提升。
                                Some(existing) if score > existing.raw_score => {
                                    existing.raw_score = score;
                                    existing.source = "context_comp";
                                }
                                Some(_) => {}
                                // 尾字不在候选(罕见):直接加入。
                                None => candidates.push(ScoredCandidate {
                                    text: tail.to_string(),
                                    family: "pinyin",
                                    source: "context_comp",
                                    raw_score: score,
                                }),
                            }
                        }
                    }
                }
            }
        }
        drop(last);

        candidates.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinyin_family_nihao() {
        let fam = PinyinFamily::new();
        let cands = fam.predict("nihao");
        assert!(!cands.is_empty());
        assert!(cands.iter().any(|c| c.text.contains("你好")));
    }

    #[test]
    fn pinyin_family_xiayige() {
        let fam = PinyinFamily::new();
        let cands = fam.predict("xiayige");
        assert!(cands.iter().any(|c| c.text == "下一个"));
    }

    #[test]
    fn phrase_book_recall_and_count_ranking() {
        // lisa→丽萨 已在 inputx 词典 —— 修复后词典词不再进单词本,
        // 这里用真正的自造词 lizhengming→李正明。
        let fam = PinyinFamily::new();
        fam.learn_phrase("lizhengming", "李正明");
        let cands = fam.predict("lizhengming");
        let p = cands.iter().find(|c| c.text == "李正明")
            .expect("learned phrase recallable");
        // 首次 0.70 —— 低于词典精确分,自造词不再强制置顶。
        assert!((p.raw_score - 0.70).abs() < 1e-9, "first use = 0.70: {}", p.raw_score);

        // 多次使用 → count 递增 → 分数随使用频率上升(0.70 + 0.02×2 = 0.74)。
        fam.learn_phrase("lizhengming", "李正明");
        fam.learn_phrase("lizhengming", "李正明");
        let cands = fam.predict("lizhengming");
        let p = cands.iter().find(|c| c.text == "李正明").unwrap();
        assert!((p.raw_score - 0.74).abs() < 1e-9, "count 3 → 0.74: {}", p.raw_score);
    }

    #[test]
    fn context_prefix_association_boosts_tail_word() {
        // 前缀整词联想:提交 中(zhong)后输入 de → "zhongde" → 中的(9307)
        // → 尾字"的"以整词权重提升(source = context_comp)。
        use crate::family::CandidateFamily;
        let path = format!("/tmp/swift-ime-ctx-{}.fst", std::process::id());
        let mut b = inputx_fsa::DictBuilder::new();
        b.insert(b"zhongde", "中的".as_bytes(), 9307);
        b.insert(b"de", "的".as_bytes(), 200_000); // 单字 freq 更高 → 不提升
        std::fs::write(&path, b.finish()).unwrap();
        let fam = PinyinFamily::new();
        fam.load_dict(&path).unwrap();
        CandidateFamily::record_pick(&fam, "zhong", "中");
        let cands = fam.predict_with_context("de", &InputContext::new());
        let di = cands.iter().find(|c| c.text == "的").expect("的 present");
        assert!(di.raw_score >= 0.68, "整词权重提升: {}", di.raw_score);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}.idx"));
    }

    #[test]
    fn context_association_off_when_context_aware_disabled() {
        use crate::family::CandidateFamily;
        let path = format!("/tmp/swift-ime-ctxoff-{}.fst", std::process::id());
        let mut b = inputx_fsa::DictBuilder::new();
        b.insert(b"zhongde", "中的".as_bytes(), 9307);
        b.insert(b"de", "的".as_bytes(), 200_000);
        std::fs::write(&path, b.finish()).unwrap();
        let fam = PinyinFamily::new();
        fam.load_dict(&path).unwrap();
        CandidateFamily::record_pick(&fam, "zhong", "中");
        fam.set_context_aware(false);
        let cands = fam.predict_with_context("de", &InputContext::new());
        let di = cands.iter().find(|c| c.text == "的").expect("的 present");
        assert_ne!(di.source, "context_comp", "关闭后无整词联想");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}.idx"));
    }

    #[test]
    fn emoji_candidates_are_not_learned_into_phrase_book() {
        // 提交 emoji(cd→📀)不该把它学进拼音单词本 —— 否则它成了 pinyin
        // 候选,吃 phrase + recent 双重加成霸榜(0.945)。emoji 有自己的
        // 关键词表,不缺这条学习路径。
        let fam = PinyinFamily::new();
        let before = fam.phrase_count();
        use crate::family::CandidateFamily;
        CandidateFamily::record_pick(&fam, "cd", "📀");
        assert_eq!(fam.phrase_count(), before, "emoji must not enter the phrase book");
        // 自生词路径同样拒收。
        fam.learn_composed_phrase("cd", "📀");
        assert_eq!(fam.phrase_count(), before, "composed path also rejects emoji");
        // 汉字 + ASCII 混合词正常学习。
        fam.learn_composed_phrase("bevyyinqing", "Bevy引擎");
        assert_eq!(fam.phrase_count(), before + 1, "mixed CJK+ASCII word is learnable");
    }

    #[test]
    fn dictionary_words_are_not_learned() {
        // de→的 在 rime-ice/inputx 词典里 —— learn_phrase 必须跳过,
        // 否则每个常用词都会被塞进单词本(修复前的行为)。
        let fam = PinyinFamily::new();
        let before = fam.phrase_count(); // 含 default_phrases 预置
        fam.learn_phrase("de", "的");
        assert_eq!(fam.phrase_count(), before,
            "dictionary word must not enter the phrase book");
    }

    #[test]
    fn returns_scored_candidates() {
        let fam = PinyinFamily::new();
        let cands = fam.predict("nihao");
        for c in &cands {
            assert!(c.raw_score >= 0.0 && c.raw_score <= 1.0);
            assert_eq!(c.family, "pinyin");
        }
    }
}
