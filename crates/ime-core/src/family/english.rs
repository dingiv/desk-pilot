//! EnglishFamily — English word prediction via prefix matching.
//!
//! # Dictionary layers
//!
//! Three-layer design:
//! 1. **base** — embedded SCOWL words (compiled into binary, pre-normalized).
//! 2. **user** — user custom dictionary (`en_user.tsv`), loaded at runtime.
//! 3. **external** — large raw-frequency word lists, dynamically normalized at load.
//!
//! # Frequency normalization
//!
//! Different sources use different scales. We normalize to a common 1-10000
//! range using **decile normalization**:
//!
//! 1. Sort all words by raw frequency (descending).
//! 2. Divide into 10 equal groups.
//! 3. Within each group i (base = i×1000):
//!    `score = base + round((freq - min) / (max - min) × 1000)`
//!
//! Dict types (declared via `# @type:` header comment):
//! - `grade`: SCOWL levels (10-95), mapped to fixed scores.
//! - `frequency`: hermitdave-style raw counts, decile-normalized.
//! - `user`: user-managed, all words score 10000.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::pinyin::recency::RecentStore;
use super::{now_ms, CandidateFamily, InputContext, ScoredCandidate};

// ── EnglishWeights ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EnglishWeights {
    pub exact: f64,
    pub prefix_ratio: f64,
    pub user_boost: f64,
    /// prefix 质量式的地板分(`0.60 地板`,与 emoji 前缀基础对齐)。
    pub prefix_base: f64,
    /// prefix 质量式的质量系数(词频 × 匹配率在 [地板, 地板+系数] 内区分)。
    pub prefix_quality: f64,
    /// 1~2 字母短词降权倍率(只作用词典层;用户层恒 1.0)。
    pub short_word_penalty: f64,
}

impl Default for EnglishWeights {
    fn default() -> Self {
        EnglishWeights {
            exact: 0.88,
            prefix_ratio: 0.60,
            user_boost: 1.0,
            prefix_base: 0.60,
            prefix_quality: 0.25,
            short_word_penalty: 0.6,
        }
    }
}

/// 单字母输入的 prefix 层工作量上限:二分定位后要扫全部同首字母词
/// ("a" ≈ 数千条),没有这道预过滤单键延迟会炸。**引擎预过滤**,非语义
/// 截断 —— 语义截断唯一入口是 `UnifiedScorer` 的 `top_n`。
const PREFILTER_TAKE: usize = 16;

// ── Frequency normalization ─────────────────────────────────────────────

/// Detected dictionary type from the `# @type:` header.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DictType {
    Grade,
    Frequency,
    User,
    /// 缓存文件专用:行内分数已是归一化终值,**原样通过**、不再映射。
    /// (修复 B1:曾误用 Grade 当 passthrough,`grade_to_score` 把归一化
    /// 分数塌缩到 2000/5500/8000/9500 四档,重启后前缀排序退化。)
    Passthrough,
}

/// Detect dict type from header lines (first 3 lines).
fn detect_dict_type(data: &str) -> DictType {
    for line in data.lines().take(3) {
        if line.contains("@type: grade") {
            return DictType::Grade;
        }
        if line.contains("@type: frequency") {
            return DictType::Frequency;
        }
        if line.contains("@type: user") {
            return DictType::User;
        }
    }
    DictType::Frequency // default for unknown sources
}

/// SCOWL grade → score mapping.
fn grade_to_score(grade: u32) -> u32 {
    match grade {
        10 => 10000,
        20 => 9000,
        35 => 7000,
        40 => 6000,
        50 => 5000,
        55 => 4500,
        60 => 4000,
        70 => 3000,
        80 => 2000,
        95 => 1000,
        g if g < 20 => 9500,
        g if g < 35 => 8000,
        g if g < 50 => 5500,
        _ => 2000,
    }
}

/// Decile normalization: 10 equal groups, linear interpolation within each group.
fn decile_normalize(mut entries: Vec<(String, u32)>) -> Vec<(String, u32)> {
    if entries.is_empty() {
        return entries;
    }

    // Sort descending by raw frequency.
    entries.sort_by_key(|(_, f)| std::cmp::Reverse(*f));

    let total = entries.len();
    let group_size = (total / 10).max(1);
    let num_groups = 10;

    let mut result = Vec::with_capacity(total);
    for group_idx in 0..num_groups {
        let start = group_idx * group_size;
        if start >= total {
            break;
        }
        let end = if group_idx == num_groups - 1 {
            total
        } else {
            ((group_idx + 1) * group_size).min(total)
        };
        let chunk = &entries[start..end];
        if chunk.is_empty() {
            continue;
        }

        let group_base = ((num_groups - 1 - group_idx) as u32) * 1000;

        let min_freq = chunk.last().map(|(_, f)| *f).unwrap_or(0);
        let max_freq = chunk.first().map(|(_, f)| *f).unwrap_or(0);
        let range = max_freq.saturating_sub(min_freq);

        for (word, freq) in chunk {
            let score = if range == 0 {
                group_base + 500 // all same freq → middle of group
            } else {
                let ratio = (freq - min_freq) as f64 / range as f64;
                group_base + (ratio * 1000.0).round() as u32
            };
            result.push((word.clone(), score.clamp(1, 10000)));
        }
    }

    // Sort alphabetically for binary search.
    sort_case_insensitive(&mut result);
    result
}

/// 大小写不敏感排序:词表里可能有专有名词(iPhone、NASA),按小写键排序,
/// 二分查找(同样按小写比较)才一致。
fn sort_case_insensitive(words: &mut [(String, u32)]) {
    words.sort_by_key(|(w, _)| w.to_ascii_lowercase());
}

// ── EnglishFamily ───────────────────────────────────────────────────────

pub struct EnglishFamily {
    /// 运行时开关(AtomicBool:trait `set_family_enabled` 经 `&self` 写入)。
    enabled: AtomicBool,
    base_words: Vec<(String, u32)>,
    user_words: Mutex<Vec<(String, u32)>>,
    priority: u32,
    weights: EnglishWeights,
    /// 近期使用加权(E2):刚提交过的英文词在 prefix/exact 候选里获得
    /// recency 合成(z = (1-a)(a+b)/8 + a,天然 <1)。复用拼音侧 RecentStore
    /// 的五档时间指数;进程内生命周期,不持久化。
    recency: Mutex<RecentStore>,
    /// 上下文感知开关(`input.context_aware`,与拼音共用同一个 yaml 键)。
    context_aware: Mutex<bool>,
    /// 持久化句柄(英文自生词 en_user 表),init_store 后由 dispatcher 注入。
    store: Mutex<Option<Arc<crate::store::WeightStore>>>,
}

impl EnglishFamily {
    pub fn new() -> Self {
        EnglishFamily {
            enabled: AtomicBool::new(true),
            base_words: Vec::new(),
            user_words: Mutex::new(Vec::new()),
            priority: 70,
            weights: EnglishWeights::default(),
            recency: Mutex::new(RecentStore::new()),
            context_aware: Mutex::new(true),
            store: Mutex::new(None),
        }
    }

    pub fn with_default_dict() -> Self {
        let mut fam = Self::new();
        let count = fam.load_into_base(Self::EMBEDDED_EN_DICT);
        if count > 0 {
            tracing::info!(count, "english: loaded embedded dictionary");
        }
        fam
    }

    /// Load TSV data into the base word list (for the embedded dict).
    fn load_into_base(&mut self, data: &[u8]) -> usize {
        self.base_words = Self::parse_and_normalize(data, DictType::Grade);
        self.base_words.len()
    }

    /// Parse TSV data, detect type, normalize, return sorted word list.
    fn parse_and_normalize(data: &[u8], dict_type: DictType) -> Vec<(String, u32)> {
        let s = match std::str::from_utf8(data) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        // key = 小写(去重/排序键),value = (原始大小写, 分数)。词表里的
        // 专有名词(iPhone、NASA)保留原始大小写,匹配时大小写不敏感,
        // 提交时回词典原始大小写。
        let mut map: std::collections::HashMap<String, (String, u32)> =
            std::collections::HashMap::new();
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            let word = parts[0].trim().to_string();
            if word.is_empty() || word.len() < 2 {
                continue;
            }
            let key = word.to_ascii_lowercase();
            let raw: u32 = parts
                .get(1)
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(100);
            let score = match dict_type {
                DictType::Grade => grade_to_score(raw),
                DictType::Frequency | DictType::User | DictType::Passthrough => raw,
            };
            match map.get_mut(&key) {
                Some((_, sc)) if score > *sc => *sc = score,
                Some(_) => {}
                None => {
                    map.insert(key, (word, score));
                }
            }
        }

        if map.is_empty() {
            return Vec::new();
        }

        let entries: Vec<(String, u32)> = map.into_values().collect();

        match dict_type {
            DictType::Grade | DictType::Passthrough => {
                let mut w = entries;
                sort_case_insensitive(&mut w);
                w
            }
            DictType::Frequency => decile_normalize(entries),
            DictType::User => {
                let mut w: Vec<_> = entries.into_iter().map(|(w, _)| (w, 10000u32)).collect();
                sort_case_insensitive(&mut w);
                w
            }
        }
    }

    pub fn with_config(mut self, priority: u32, weights: EnglishWeights) -> Self {
        self.priority = priority;
        self.weights = weights;
        self
    }

    // ── Dictionary loading ─────────────────────────────────────────────

    /// 学习一个英文自生词:Enter 强制提交 raw 文本(如 cd)时调用。
    /// 内存进 user 层(exact 0.88 × priority 70 ≈ 0.62,压过 emoji 前缀与
    /// 中文简拼)+ SQLite en_user 表持久化。
    ///
    /// **是否该学由提交点的候选来源决定**(引擎侧):空格/数字提交英文候选
    /// (来源 exact/prefix/user)不调这里;只有 raw 提交(Enter 强选)才学。
    pub fn record_learned_word(&self, word: &str) {
        if word.is_empty() || !word.chars().all(|c| c.is_ascii_alphanumeric()) {
            return;
        }
        // 保留原始大小写:专有名词(iPhone、NASA)匹配时大小写不敏感,
        // 提交时回词典原始大小写。
        self.merge_into_user(&[(word.to_string(), 10_000)]);
        if let Some(ref store) = *self.store.lock().unwrap() {
            store.record_en_user(word);
        }
    }

    /// Warm the user layer from persisted 英文自生词。
    pub fn warm_learned_words(&self, words: &[(String, u32)]) {
        if words.is_empty() {
            return;
        }
        self.merge_into_user(words);
    }

    // ── E2:近期使用加权(recency)──

    /// Record a committed word(引擎提交路径按家族分派到这里)。
    pub fn record_commit(&self, word: &str) {
        self.recency.lock().unwrap().record(word, now_ms());
    }

    /// 临时关闭/恢复上下文感知(recency boost;`input.context_aware` 与
    /// 拼音家族共用)。
    pub fn set_context_aware(&self, on: bool) {
        *self.context_aware.lock().unwrap() = on;
    }

    /// 近期指数合成(z = (1-a)(a+b)/8 + a)施加到全部候选 —— 排序前调用。
    /// 与拼音家族 Layer 1 同公式:增量与 (1-a) 成比例,z 天然 < 1。
    fn apply_recency(&self, out: &mut [ScoredCandidate]) {
        if !*self.context_aware.lock().unwrap() {
            return;
        }
        let mut recency = self.recency.lock().unwrap();
        if recency.is_empty() {
            return;
        }
        let now = now_ms();
        for c in out.iter_mut() {
            let b = recency.tier(&c.text, now);
            if b > 0 {
                let a = c.raw_score;
                c.raw_score = (1.0 - a) * (a + b as f64) / 8.0 + a;
            }
        }
    }

    /// Merge `words` into the user word layer(大小写不敏感去重:小写为键,
    /// 分数取最大,同分时后见的大小写胜出)。
    fn merge_into_user(&self, words: &[(String, u32)]) {
        let mut user = self.user_words.lock().unwrap();
        let mut merged: std::collections::HashMap<String, (String, u32)> = user
            .iter()
            .map(|(w, s)| (w.to_ascii_lowercase(), (w.clone(), *s)))
            .collect();
        for (w, s) in words {
            let key = w.to_ascii_lowercase();
            let entry = merged.entry(key).or_insert_with(|| (w.clone(), 0));
            if *s >= entry.1 {
                entry.0 = w.clone(); // 后见的大小写胜出
                entry.1 = *s;
            }
        }
        *user = merged.into_values().collect();
        sort_case_insensitive(&mut user);
    }

    /// Load from file path. Auto-detects dict type from `# @type:` header.
    /// Uses a `.en_cache` file to avoid re-normalizing on every startup.
    pub fn load_dict_file(&self, path: &str) -> std::io::Result<usize> {
        let data = std::fs::read(path)?;

        // ── Check cache ──
        let file_hash = Self::hash_bytes(&data);
        let cache_path = format!("{path}.en_cache");
        if let Ok(words) = Self::load_cache_if_valid(&cache_path, path, file_hash) {
            let count = words.len();
            self.merge_into_user(&words);
            tracing::info!(count, path, "english: loaded from cache");
            return Ok(count);
        }

        // ── Parse, normalize, cache ──
        let dict_type = detect_dict_type(std::str::from_utf8(&data).unwrap_or(""));
        let words = Self::parse_and_normalize(&data, dict_type);
        let count = words.len();

        let _ = Self::write_cache(&cache_path, path, file_hash, &words);

        self.merge_into_user(&words);
        tracing::info!(count, path, ?dict_type, "english: loaded + cached");
        Ok(count)
    }

    // ── Cache helpers ─────────────────────────────────────────────────

    fn hash_bytes(data: &[u8]) -> u64 {
        let mut h = DefaultHasher::new();
        data.hash(&mut h);
        h.finish()
    }

    /// Cache format (v2 — v1 曾把缓存内容错走 grade_to_score 重映射,B1 修复时
    /// 升版:旧头校验必然失败 → 缓存自动重建,存量污染自愈):
    ///   # @cache: v2 <source_path> <hash>
    ///   word\tfreq
    ///   ...
    fn write_cache(
        cache_path: &str,
        source_path: &str,
        hash: u64,
        words: &[(String, u32)],
    ) -> std::io::Result<()> {
        let mut f = std::fs::File::create(cache_path)?;
        writeln!(f, "# @cache: v2 {source_path} {hash}")?;
        for (w, s) in words {
            writeln!(f, "{w}\t{s}")?;
        }
        Ok(())
    }

    fn load_cache_if_valid(
        cache_path: &str,
        source_path: &str,
        expected_hash: u64,
    ) -> Result<Vec<(String, u32)>, ()> {
        let data = std::fs::read(cache_path).map_err(|_| ())?;
        let s = std::str::from_utf8(&data).map_err(|_| ())?;

        // Validate header: must match version, source path and hash.
        let first_line = s.lines().next().ok_or(())?;
        let expected = format!("# @cache: v2 {source_path} {expected_hash}");
        if first_line.trim() != expected {
            return Err(());
        }

        // Parse word list from cache — 分数已是归一化终值,Passthrough 原样通过。
        let words = Self::parse_and_normalize(&data, DictType::Passthrough);
        if words.is_empty() {
            return Err(());
        }
        Ok(words)
    }

    /// Load a user dictionary file (all words get max priority, 10000).
    pub fn load_user_dict_file(&self, path: &str) -> std::io::Result<usize> {
        let data = std::fs::read(path)?;
        let words = Self::parse_and_normalize(&data, DictType::User);
        let count = words.len();
        self.merge_into_user(&words);
        tracing::info!(count, path, "english: loaded user dict");
        Ok(count)
    }

    /// Embedded base dict (SCOWL, pre-normalized).
    const EMBEDDED_EN_DICT: &[u8] =
        include_bytes!("../../../../apps/swift-ime/assets/dict/en_words.tsv");

    // ── Frequency-to-score ─────────────────────────────────────────────

    /// 词频(1~10000,decile 归一化)→ 分档质量分。
    /// 与 lattice 的 `freq_to_score`(log₂ 连续映射)是两套刻度 —— 见
    /// weight-scoring.md 的分数来源对照表。
    fn frequency_band(freq: u32) -> f64 {
        match freq {
            f if f >= 9000 => 0.90,
            f if f >= 7000 => 0.70,
            f if f >= 5000 => 0.50,
            f if f >= 3000 => 0.35,
            _ => 0.25,
        }
    }

    // ── Query helpers ──────────────────────────────────────────────────

    /// 私有查询助手(标签×2 + 双入参),不对外 —— 参数数是刻意的展开,
    /// 不值得为消 lint 引入配置结构体。`user_layer`:exact×user_boost、
    /// prefix_ratio×1.1、短词不降权(学习语义保留)。
    #[allow(clippy::too_many_arguments)]
    fn query_layer(
        words: &[(String, u32)],
        input: &str,
        exact_label: &'static str,
        prefix_label: &'static str,
        w: &EnglishWeights,
        user_layer: bool,
        short_penalty: bool,
        seen: &mut std::collections::HashSet<String>,
        out: &mut Vec<ScoredCandidate>,
    ) {
        let exact_score = if user_layer {
            (w.exact * w.user_boost).min(1.0)
        } else {
            w.exact
        };
        let prefix_ratio = if user_layer {
            w.prefix_ratio * 1.1
        } else {
            w.prefix_ratio
        };
        let start = words
            .binary_search_by(|(w, _)| {
                let wl = w.to_ascii_lowercase();
                if wl.as_str() < input {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            })
            .unwrap_err();

        for (word, freq) in words[start..].iter() {
            let wl = word.to_ascii_lowercase();
            if !wl.starts_with(input) {
                break;
            }
            if !seen.insert(wl.clone()) {
                continue;
            }

            // 1~2 字母短词降权(词典层):两字母输入几乎总是中文简拼,
            // 英文短词不该压过它们。
            let short = short_penalty && word.chars().count() <= 2;

            if wl == input {
                let score = if short {
                    exact_score * w.short_word_penalty
                } else {
                    exact_score
                };
                out.push(ScoredCandidate {
                    text: word.clone(),
                    family: "english",
                    source: exact_label,
                    raw_score: score,
                });
            } else {
                let freq_score = Self::frequency_band(*freq);
                let len_ratio = input.len() as f64 / word.len() as f64;
                // 地板 + 质量:0.60 地板与 emoji 前缀基础对齐(英文本尊
                // clea→clean 必须压过同名词的 emoji 关键词 clean→🧼;经
                // priority 70 后 0.42,高于 emoji 前缀 0.36、低于中文简拼
                // 0.503),词频 × 匹配率在 [0.60, 0.85] 内提供区分度 ——
                // smile(匹配 4/5)排在 smilacaceous(4/13)前,而非全体
                // 贴地板后退化成字母序。
                let base =
                    w.prefix_base + w.prefix_quality * freq_score * prefix_ratio * len_ratio;
                let score = if short {
                    base * w.short_word_penalty
                } else {
                    base
                };
                out.push(ScoredCandidate {
                    text: word.clone(),
                    family: "english",
                    source: prefix_label,
                    raw_score: score,
                });
            }
        }
    }
}

impl Default for EnglishFamily {
    fn default() -> Self {
        Self::with_default_dict()
    }
}

impl CandidateFamily for EnglishFamily {
    fn name(&self) -> &'static str {
        "english"
    }
    fn priority(&self) -> u32 {
        self.priority
    }
    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    /// 运行时开关(修复 B3:此前默认 no-op,禁用静默无效)。
    fn set_family_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
    }
    fn top_n(&self) -> usize {
        8
    }

    fn predict(&self, input: &str, _ctx: &InputContext) -> Vec<ScoredCandidate> {
        let _ = _ctx; // recency 表是家族自有的(E2),ctx 预留给未来跨会话上下文
        // 大小写不敏感:输入(通常已是小写 buffer)归一小写后匹配。
        if input.is_empty() || !input.chars().all(|c| c.is_ascii_alphabetic()) {
            return Vec::new();
        }

        let input_lower = input.to_ascii_lowercase();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // ── Member: self/case(单字母输入)── 字母本尊 + 大小写互换,家族内
        // 最优先。中英混输里单字母的意图几乎总是字母本身,而不是等它长成
        // 单词。词典层对 <2 字母无 exact(词表 len<2 被裁),prefix 照常
        // 给出 —— 单字母开头的词排其后("3. ...")。
        if input_lower.chars().count() == 1 {
            let ch = input_lower.chars().next().unwrap();
            let swapped = ch.to_ascii_uppercase().to_string();
            out.push(ScoredCandidate {
                text: input_lower.clone(),
                family: "english",
                source: "self",
                raw_score: 1.0,
            });
            out.push(ScoredCandidate {
                text: swapped.clone(),
                family: "english",
                source: "case",
                raw_score: 0.92,
            });
            seen.insert(input_lower.clone());
            seen.insert(swapped);
        }

        // Layer 1: user dict (highest priority). 自生词不降权(学习语义保留)。
        let user = self.user_words.lock().unwrap();
        Self::query_layer(
            &user,
            &input_lower,
            "user",
            "user_prefix",
            &self.weights,
            true,
            false,
            &mut seen,
            &mut out,
        );
        drop(user);

        // Layer 2: base dict. 1~2 字母短词降权。
        Self::query_layer(
            &self.base_words,
            &input_lower,
            "exact",
            "prefix",
            &self.weights,
            false,
            true,
            &mut seen,
            &mut out,
        );

        // ── E2:近期使用加权(排序前;刚提交过的词浮上来)──
        self.apply_recency(&mut out);

        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out.truncate(PREFILTER_TAKE);
        out
    }

    fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        self.load_dict_file(path)
    }

    fn attach_store(&self, store: Arc<crate::store::WeightStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    // ── 家族私有能力(D5 接口隔离:不在 CandidateFamily trait 上)──
    // record_learned_word / warm_learned_words —— 见上方固有 impl 的 pub
    // 方法,经引擎直持的具体句柄直调。

    fn load_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.load_user_dict_file(path)
    }
}

/// Arc 共享句柄的 trait 委托(D5):引擎持 `Arc<EnglishFamily>` 直调
/// 家族私有方法,scorer 持同一 Arc 当 trait 对象参与统一排序。
impl CandidateFamily for std::sync::Arc<EnglishFamily> {
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
    fn attach_store(&self, store: std::sync::Arc<crate::store::WeightStore>) {
        (**self).attach_store(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per-test temp dict — tests run in parallel threads, and a shared
    /// `en_test_{pid}.tsv` made writers race (one test's content could be
    /// overwritten before the other read it → flaky `user_dict_overrides_base`).
    fn temp_dict(tag: &str, content: &str) -> String {
        let path = std::env::temp_dir().join(format!("en_test_{}_{tag}.tsv", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn english_recency_lifts_recently_committed_word() {
        // E2:刚提交过的词在候选里获得 recency 合成(z = (1-a)(a+b)/8 + a,
        // 天然 <1 不顶满);关闭上下文感知后 boost 消失。
        let fam = EnglishFamily::with_default_dict();
        let base = fam.predict("prese", &InputContext::new());
        let p_base = base.iter().find(|c| c.text == "present").expect("present in dict");
        let a = p_base.raw_score;
        assert!(a > 0.0 && a < 1.0);

        fam.record_commit("present");
        let ctx = fam.predict("prese", &InputContext::new());
        let p_ctx = ctx.iter().find(|c| c.text == "present").unwrap();
        assert!(
            p_ctx.raw_score > a,
            "recency lifts: {} → {}",
            a, p_ctx.raw_score
        );
        assert!(p_ctx.raw_score < 1.0, "z < 1(不顶满): {}", p_ctx.raw_score);

        // 排序影响:present 与 presented(同 band 同长度)base 时相邻,
        // recency 后 present 应压过它。
        let pr_base = base.iter().find(|c| c.text == "presented").unwrap().raw_score;
        let pr_ctx = ctx.iter().find(|c| c.text == "presented").unwrap().raw_score;
        assert!(a >= pr_base, "短词 base 更高: {} vs {}", a, pr_base);
        assert!(
            p_ctx.raw_score > pr_ctx,
            "recent word outranks peer: {} vs {}",
            p_ctx.raw_score, pr_ctx
        );

        // gate:context_aware 关闭 → 无 boost。
        fam.set_context_aware(false);
        let off = fam.predict("prese", &InputContext::new());
        let p_off = off.iter().find(|c| c.text == "present").unwrap();
        assert!((p_off.raw_score - a).abs() < 1e-9, "gate off: {}", p_off.raw_score);
    }

    #[test]
    fn decile_distributes_scores() {
        // 100 words with declining freq: word0=100000, word99=1
        let entries: Vec<(String, u32)> = (0..100)
            .map(|i| {
                (
                    format!("w{i:03}"),
                    100000u32.saturating_sub(i as u32 * 1000),
                )
            })
            .collect();
        let result = decile_normalize(entries);
        // Groups should have ascending base scores.
        assert_eq!(result.len(), 100);
        // First word (highest freq) should be in group 9 → score near 10000.
        let w000 = result.iter().find(|(w, _)| w == "w000").unwrap();
        assert!(
            w000.1 >= 9000,
            "top word should be in group 9, got {}",
            w000.1
        );
        // Last word (lowest freq) should be in group 0 → score near 0.
        let w099 = result.iter().find(|(w, _)| w == "w099").unwrap();
        assert!(
            w099.1 < 1000,
            "last word should be in group 0, got {}",
            w099.1
        );
    }

    #[test]
    fn single_letter_self_and_case_lead() {
        // 单字母输入:字母本尊 + 大小写互换置顶(self/case 成员),字典
        // prefix 候选(单字母开头的词)排其后。
        let fam = EnglishFamily::with_default_dict();
        let cands = fam.predict("a", &InputContext::new());
        assert_eq!(cands[0].text, "a");
        assert_eq!(cands[0].source, "self");
        assert_eq!(cands[1].text, "A");
        assert_eq!(cands[1].source, "case");
        assert!(cands.len() > 2, "prefix 候选照常跟随: {:?}", cands.len());
        assert!(
            cands[2..].iter().all(|c| c.text.to_lowercase().starts_with('a')),
            "其余候选仍以该字母开头"
        );
    }

    #[test]
    fn exact_match_black() {
        let fam = EnglishFamily::with_default_dict();
        let cands = fam.predict("black", &InputContext::new());
        assert!(cands
            .iter()
            .any(|c| c.text == "black" && c.raw_score > 0.85));
    }

    #[test]
    fn common_words_found() {
        let fam = EnglishFamily::with_default_dict();
        for word in &[
            "hello", "world", "python", "code", "data", "server", "language", "computer",
        ] {
            let cands = fam.predict(word, &InputContext::new());
            assert!(
                cands.iter().any(|c| c.text == *word),
                "{word} should be in dict, got: {:?}",
                cands.iter().map(|c| &c.text).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn user_dict_overrides_base() {
        let fam = EnglishFamily::with_default_dict();
        fam.load_user_dict_file(&temp_dict("user", "github\nkubernetes\n"))
            .unwrap();
        let cands = fam.predict("github", &InputContext::new());
        assert!(cands
            .iter()
            .any(|c| c.text == "github" && c.source == "user"));
    }

    #[test]
    fn load_external_decile_normalizes() {
        let fam = EnglishFamily::with_default_dict();
        // Create a fake frequency dict
        let d = temp_dict(
            "decile",
            "# @type: frequency\nhello\t100000\nworld\t50000\nzzz\t1\n",
        );
        fam.load_dict_file(&d).unwrap();
        let cands = fam.predict("hello", &InputContext::new());
        // Should be in user layer now, with decile-normalized score.
        assert_eq!(cands[0].text, "hello");
        // zzz should also be loaded but with low decile score.
        let z = fam.predict("zzz", &InputContext::new());
        assert!(!z.is_empty());
    }

    #[test]
    fn cache_reload_preserves_scores() {
        // B1 回归:缓存重载必须与首次加载分数完全一致。曾用 Grade 重Parse
        // 缓存,decile 归一化分数(任意 1~10000)被 grade_to_score 塌缩到
        // 2000/5500/8000/9500 四档 —— 重启后前缀排序退化。
        let fam = EnglishFamily::with_default_dict();
        // 12 个词、频率拉开差距 → decile 归一化后分数落在多个档位。
        let content: String = (0..12)
            .map(|i| format!("w{i:02}\t{}\n", 100_000u32 >> i))
            .collect();
        let d = temp_dict("cache-b1", &format!("# @type: frequency\n{content}"));

        fam.load_dict_file(&d).unwrap(); // 首次:parse + 写缓存
        let first: Vec<(String, u32)> = fam.user_words.lock().unwrap().clone();

        fam.load_dict_file(&d).unwrap(); // 二次:命中缓存
        let second: Vec<(String, u32)> = fam.user_words.lock().unwrap().clone();

        assert_eq!(first, second, "缓存重载后分数必须与首次加载一致");
        // 钉死档位多样性:若修复回退(分数塌缩),中间档会被抹平。
        let distinct: std::collections::HashSet<u32> = first.iter().map(|(_, s)| *s).collect();
        assert!(
            distinct.len() >= 4,
            "decile 归一化应产生多档分数, got {distinct:?}"
        );
    }

    #[test]
    fn legacy_cache_header_invalidates() {
        // v1 缓存(无版本号)必须被视为无效 → 触发重建,存量污染自愈。
        let fam = EnglishFamily::with_default_dict();
        let d = temp_dict("legacy", "# @type: frequency\nhello\t100000\n");
        let cache_path = format!("{d}.en_cache");
        std::fs::write(&cache_path, "# @cache: /stale/path 12345\nhello\t2000\n").unwrap();

        fam.load_dict_file(&d).unwrap(); // v1 头校验失败 → 重 parse + 重写缓存
        let first = std::fs::read_to_string(&cache_path).unwrap();
        assert!(
            first.starts_with("# @cache: v2 "),
            "缓存应被重建为 v2 头, got: {first:?}"
        );
    }

    #[test]
    fn empty_returns_nothing() {
        assert!(EnglishFamily::with_default_dict()
            .predict("", &InputContext::new())
            .is_empty());
    }

    #[test]
    fn no_match_garbage() {
        assert!(EnglishFamily::with_default_dict()
            .predict("zzzzz", &InputContext::new())
            .is_empty());
    }

    #[test]
    fn uppercase_dict_words_match_case_insensitively_and_preserve_case() {
        // 词表里的专有名词(iPhone、NASA)保留原始大小写:小写输入也能
        // 匹配,候选回词典原始大小写。
        let fam = EnglishFamily::with_default_dict();
        fam.load_user_dict_file(&temp_dict("proper", "iPhone\nNASA\n"))
            .unwrap();

        let c = fam.predict("iphone", &InputContext::new());
        assert!(
            c.iter().any(|x| x.text == "iPhone"),
            "iphone → iPhone: {c:?}"
        );
        let n = fam.predict("nasa", &InputContext::new());
        assert!(n.iter().any(|x| x.text == "NASA"), "nasa → NASA: {n:?}");
        // 大写输入同样匹配(双向大小写不敏感)。
        let c2 = fam.predict("IPHONE", &InputContext::new());
        assert!(
            c2.iter().any(|x| x.text == "iPhone"),
            "IPHONE → iPhone: {c2:?}"
        );
    }

    #[test]
    fn learned_word_preserves_case() {
        let fam = EnglishFamily::new(); // 空 base,只看 learned 层
        fam.record_learned_word("iPhone");
        let c = fam.predict("iphone", &InputContext::new());
        assert!(
            c.iter().any(|x| x.text == "iPhone"),
            "learned iPhone matches iphone: {c:?}"
        );
    }

    #[test]
    fn short_base_words_are_penalized() {
        // 词典里的 1~2 字母短词降权:exact 0.88 → 0.88×0.6=0.528(两字母输入
        // 几乎总是中文简拼,英文短词不该压过它们)。长词不受影响。
        let mut fam = EnglishFamily::new();
        fam.load_into_base(b"cd\t3000\ncat\t1000\n");
        let cands = fam.predict("cd", &InputContext::new());
        let cd = cands.iter().find(|c| c.text == "cd").expect("cd in base");
        assert!(
            (cd.raw_score - 0.88 * 0.6).abs() < 1e-9,
            "2-letter word penalized: {}",
            cd.raw_score,
        );

        let cands = fam.predict("cat", &InputContext::new());
        let cat = cands.iter().find(|c| c.text == "cat").expect("cat in base");
        assert_eq!(cat.raw_score, 0.88, "long word keeps exact score");
    }

    #[test]
    fn learned_short_words_not_penalized() {
        // 自生词(用户层)不降权:学入 "cd" 后仍以全权重出现。
        let fam = EnglishFamily::new(); // 空 base,只看 learned 层
        fam.record_learned_word("cd");
        let cands = fam.predict("cd", &InputContext::new());
        let c = cands
            .iter()
            .find(|x| x.text == "cd")
            .expect("learned cd surfaces");
        assert_eq!(c.raw_score, 0.88, "learned short word keeps full weight");
    }
}
