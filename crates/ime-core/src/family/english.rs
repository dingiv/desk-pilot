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
use std::sync::{Arc, Mutex};

use super::{CandidateFamily, ScoredCandidate};

// ── EnglishWeights ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EnglishWeights {
    pub exact: f64,
    pub prefix_ratio: f64,
    pub user_boost: f64,
}

impl Default for EnglishWeights {
    fn default() -> Self {
        EnglishWeights { exact: 0.88, prefix_ratio: 0.60, user_boost: 1.0 }
    }
}

// ── Frequency normalization ─────────────────────────────────────────────

/// Detected dictionary type from the `# @type:` header.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DictType { Grade, Frequency, User }

/// Detect dict type from header lines (first 3 lines).
fn detect_dict_type(data: &str) -> DictType {
    for line in data.lines().take(3) {
        if line.contains("@type: grade") { return DictType::Grade; }
        if line.contains("@type: frequency") { return DictType::Frequency; }
        if line.contains("@type: user") { return DictType::User; }
    }
    DictType::Frequency // default for unknown sources
}

/// SCOWL grade → score mapping.
fn grade_to_score(grade: u32) -> u32 {
    match grade {
        10 => 10000, 20 => 9000, 35 => 7000, 40 => 6000,
        50 => 5000, 55 => 4500, 60 => 4000, 70 => 3000,
        80 => 2000, 95 => 1000,
        g if g < 20 => 9500,
        g if g < 35 => 8000,
        g if g < 50 => 5500,
        _ => 2000,
    }
}

/// Decile normalization: 10 equal groups, linear interpolation within each group.
fn decile_normalize(mut entries: Vec<(String, u32)>) -> Vec<(String, u32)> {
    if entries.is_empty() { return entries; }

    // Sort descending by raw frequency.
    entries.sort_by(|a, b| b.1.cmp(&a.1));

    let total = entries.len();
    let group_size = (total / 10).max(1);
    let num_groups = 10;

    let mut result = Vec::with_capacity(total);
    for group_idx in 0..num_groups {
        let start = group_idx * group_size;
        if start >= total { break; }
        let end = if group_idx == num_groups - 1 { total } else { ((group_idx + 1) * group_size).min(total) };
        let chunk = &entries[start..end];
        if chunk.is_empty() { continue; }

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
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

// ── EnglishFamily ───────────────────────────────────────────────────────

pub struct EnglishFamily {
    enabled: bool,
    base_words: Vec<(String, u32)>,
    user_words: Mutex<Vec<(String, u32)>>,
    priority: u32,
    weights: EnglishWeights,
    /// 持久化句柄(英文自生词 en_user 表),init_store 后由 dispatcher 注入。
    store: Mutex<Option<Arc<crate::weight_store::WeightStore>>>,
}

impl EnglishFamily {
    pub fn new() -> Self {
        EnglishFamily {
            enabled: true,
            base_words: Vec::new(),
            user_words: Mutex::new(Vec::new()),
            priority: 70,
            weights: EnglishWeights::default(),
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

        let mut map: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            let parts: Vec<&str> = line.split('\t').collect();
            let word = parts[0].trim().to_ascii_lowercase();
            if word.is_empty() || word.len() < 2 { continue; }
            let raw: u32 = parts.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(100);
            let score = match dict_type {
                DictType::Grade => grade_to_score(raw),
                DictType::Frequency | DictType::User => raw,
            };
            let entry = map.entry(word).or_insert(0);
            if score > *entry { *entry = score; }
        }

        if map.is_empty() { return Vec::new(); }

        let entries: Vec<(String, u32)> = map.into_iter().collect();
        let words = match dict_type {
            DictType::Grade => {
                let mut w = entries;
                w.sort_by(|a, b| a.0.cmp(&b.0));
                w
            }
            DictType::Frequency => decile_normalize(entries),
            DictType::User => {
                let mut w: Vec<_> = entries.into_iter().map(|(w, _)| (w, 10000u32)).collect();
                w.sort_by(|a, b| a.0.cmp(&b.0));
                w
            }
        };
        words
    }

    pub fn with_config(mut self, priority: u32, weights: EnglishWeights) -> Self {
        self.priority = priority;
        self.weights = weights;
        self
    }

    pub fn set_enabled(&mut self, enabled: bool) { self.enabled = enabled; }

    // ── Dictionary loading ─────────────────────────────────────────────

    /// 学习一个英文自生词:Enter 强制提交 raw 文本(如 cd)时调用。
    /// 内存进 user 层(exact 0.88 × priority 70 ≈ 0.62,压过 emoji 前缀与
    /// 中文简拼)+ SQLite en_user 表持久化。
    pub fn record_learned_word(&self, word: &str) {
        if word.is_empty() || !word.chars().all(|c| c.is_ascii_alphanumeric()) {
            return;
        }
        self.merge_into_user(&[(word.to_string(), 10_000)]);
        if let Some(ref store) = *self.store.lock().unwrap() {
            store.record_en_user(word);
        }
    }

    /// Warm the user layer from persisted 英文自生词。
    pub fn warm_learned_words(&self, words: &[(String, u32)]) {
        if words.is_empty() { return; }
        self.merge_into_user(words);
    }

    /// Merge `words` into the user word layer.
    fn merge_into_user(&self, words: &[(String, u32)]) {
        let mut user = self.user_words.lock().unwrap();
        let mut merged: std::collections::HashMap<String, u32> = user.iter().cloned().collect();
        for (w, s) in words {
            let entry = merged.entry(w.clone()).or_insert(0);
            if *s > *entry { *entry = *s; }
        }
        *user = merged.into_iter().collect();
        user.sort_by(|a, b| a.0.cmp(&b.0));
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

    /// Cache format:
    ///   # @cache: <source_path> <hash>
    ///   word\tfreq
    ///   ...
    fn write_cache(cache_path: &str, source_path: &str, hash: u64, words: &[(String, u32)]) -> std::io::Result<()> {
        let mut f = std::fs::File::create(cache_path)?;
        writeln!(f, "# @cache: {source_path} {hash}")?;
        for (w, s) in words {
            writeln!(f, "{w}\t{s}")?;
        }
        Ok(())
    }

    fn load_cache_if_valid(cache_path: &str, source_path: &str, expected_hash: u64) -> Result<Vec<(String, u32)>, ()> {
        let data = std::fs::read(cache_path).map_err(|_| ())?;
        let s = std::str::from_utf8(&data).map_err(|_| ())?;

        // Validate header: must match source path and hash.
        let first_line = s.lines().next().ok_or(())?;
        let expected = format!("# @cache: {source_path} {expected_hash}");
        if first_line.trim() != expected { return Err(()); }

        // Parse word list from cache.
        let words = Self::parse_and_normalize(&data, DictType::Grade); // Grade = Passthrough (scores already normalized)
        if words.is_empty() { return Err(()); }
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

    fn freq_to_score(freq: u32) -> f64 {
        match freq {
            f if f >= 9000 => 0.90,
            f if f >= 7000 => 0.70,
            f if f >= 5000 => 0.50,
            f if f >= 3000 => 0.35,
            _              => 0.25,
        }
    }

    // ── Query helpers ──────────────────────────────────────────────────

    fn query_layer(
        words: &[(String, u32)],
        input: &str,
        source: &'static str,
        exact_score: f64,
        prefix_ratio: f64,
        seen: &mut std::collections::HashSet<String>,
        out: &mut Vec<ScoredCandidate>,
    ) {
        let start = words.binary_search_by(|(w, _)| {
            if w.as_str() < input { std::cmp::Ordering::Less }
            else { std::cmp::Ordering::Greater }
        }).unwrap_err();

        for (word, freq) in words[start..].iter() {
            if !word.starts_with(input) { break; }
            if !seen.insert(word.clone()) { continue; }

            if *word == input {
                out.push(ScoredCandidate {
                    text: word.clone(), family: "english", source,
                    raw_score: exact_score,
                });
            } else {
                let freq_score = Self::freq_to_score(*freq);
                let len_ratio = input.len() as f64 / word.len() as f64;
                let score = (freq_score * prefix_ratio * len_ratio).clamp(0.15, 0.85);
                out.push(ScoredCandidate {
                    text: word.clone(), family: "english", source,
                    raw_score: score,
                });
            }
        }
    }
}

impl Default for EnglishFamily {
    fn default() -> Self { Self::with_default_dict() }
}

impl CandidateFamily for EnglishFamily {
    fn name(&self) -> &'static str { "english" }
    fn priority(&self) -> u32 { self.priority }
    fn enabled(&self) -> bool { self.enabled }
    fn top_n(&self) -> usize { 8 }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() || !input.chars().all(|c| c.is_ascii_lowercase()) {
            return Vec::new();
        }

        let input_lower = input.to_ascii_lowercase();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Layer 1: user dict (highest priority).
        let user = self.user_words.lock().unwrap();
        let user_exact = (self.weights.exact * self.weights.user_boost).min(1.0);
        Self::query_layer(&user, &input_lower, "user", user_exact, self.weights.prefix_ratio * 1.1, &mut seen, &mut out);
        drop(user);

        // Layer 2: base dict.
        Self::query_layer(&self.base_words, &input_lower, "exact", self.weights.exact, self.weights.prefix_ratio, &mut seen, &mut out);

        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out.truncate(16);
        out
    }

    fn load_dict(&self, path: &str) -> std::io::Result<usize> {
        self.load_dict_file(path)
    }

    fn attach_store(&self, store: Arc<crate::weight_store::WeightStore>) {
        *self.store.lock().unwrap() = Some(store);
    }

    fn record_learned_word(&self, word: &str) {
        // 委托 inherent 实现(dispatcher 经 trait 对象调用)。
        EnglishFamily::record_learned_word(self, word);
    }

    fn warm_learned_words(&self, words: &[(String, u32)]) {
        EnglishFamily::warm_learned_words(self, words);
    }

    fn load_user_dict(&self, path: &str) -> std::io::Result<usize> {
        self.load_user_dict_file(path)
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
    fn decile_distributes_scores() {
        // 100 words with declining freq: word0=100000, word99=1
        let entries: Vec<(String, u32)> = (0..100)
            .map(|i| (format!("w{i:03}"), 100000u32.saturating_sub(i as u32 * 1000)))
            .collect();
        let result = decile_normalize(entries);
        // Groups should have ascending base scores.
        assert_eq!(result.len(), 100);
        // First word (highest freq) should be in group 9 → score near 10000.
        let w000 = result.iter().find(|(w, _)| w == "w000").unwrap();
        assert!(w000.1 >= 9000, "top word should be in group 9, got {}", w000.1);
        // Last word (lowest freq) should be in group 0 → score near 0.
        let w099 = result.iter().find(|(w, _)| w == "w099").unwrap();
        assert!(w099.1 < 1000, "last word should be in group 0, got {}", w099.1);
    }

    #[test]
    fn exact_match_black() {
        let fam = EnglishFamily::with_default_dict();
        let cands = fam.predict("black");
        assert!(cands.iter().any(|c| c.text == "black" && c.raw_score > 0.85));
    }

    #[test]
    fn common_words_found() {
        let fam = EnglishFamily::with_default_dict();
        for word in &["hello", "world", "python", "code", "data", "server", "language", "computer"] {
            let cands = fam.predict(word);
            assert!(cands.iter().any(|c| c.text == *word),
                "{word} should be in dict, got: {:?}",
                cands.iter().map(|c| &c.text).collect::<Vec<_>>());
        }
    }

    #[test]
    fn user_dict_overrides_base() {
        let fam = EnglishFamily::with_default_dict();
        fam.load_user_dict_file(&temp_dict("user", "github\nkubernetes\n")).unwrap();
        let cands = fam.predict("github");
        assert!(cands.iter().any(|c| c.text == "github" && c.source == "user"));
    }

    #[test]
    fn load_external_decile_normalizes() {
        let fam = EnglishFamily::with_default_dict();
        // Create a fake frequency dict
        let d = temp_dict("decile", "# @type: frequency\nhello\t100000\nworld\t50000\nzzz\t1\n");
        fam.load_dict_file(&d).unwrap();
        let cands = fam.predict("hello");
        // Should be in user layer now, with decile-normalized score.
        assert_eq!(cands[0].text, "hello");
        // zzz should also be loaded but with low decile score.
        let z = fam.predict("zzz");
        assert!(!z.is_empty());
    }

    #[test]
    fn empty_returns_nothing() {
        assert!(EnglishFamily::with_default_dict().predict("").is_empty());
    }

    #[test]
    fn no_match_garbage() {
        assert!(EnglishFamily::with_default_dict().predict("zzzzz").is_empty());
    }
}
