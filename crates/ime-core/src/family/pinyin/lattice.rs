//! LatticeDecoder — unified pinyin matching via initials-boundary segmentation.
//!
//! Replaces the separate `dict` (exact match), `viterbi` (bigram composition),
//! and `jianpin` (initials index) with a single engine.
//!
//! Algorithm:
//! 1. Greedy-parse input by initial-consonant boundaries (声母切分).
//!    Each segment is either a complete valid syllable or a single initial letter.
//! 2. Extract segment initials → lookup in `initials_index` (HashMap, O(1)).
//! 3. Pattern-verify each candidate: do the code's syllables match the segments?
//!
//! Supports:
//! - Full pinyin:   guangyinsijian → 光阴似箭
//! - Pure initials: gysj → 光阴似箭
//! - Mixed:         guangyinsj → 光阴似箭
//! - Complex mixed: gyinsjian → 光阴似箭

use std::collections::HashMap;

/// One segment after greedy parsing.
#[derive(Debug, Clone, PartialEq)]
enum Segment {
    /// Complete valid syllable (e.g., "guang", "yin", "jian").
    Full(String),
    /// Single initial letter (e.g., "g", "s", "j").
    Initial(char),
}

impl Segment {
    fn initial(&self) -> char {
        match self {
            Segment::Full(s) => s.chars().next().unwrap(),
            Segment::Initial(c) => *c,
        }
    }
}

/// Greedy-parse input by initial-consonant boundaries.
/// Each consonant/semi-vowel starts a new segment; try to extend
/// to the longest valid syllable; if none found, keep as single initial.
fn greedy_parse(input: &str) -> Vec<Segment> {
    let chars: Vec<char> = input.chars().collect();
    let mut segments = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        // Try longest valid syllable from position i.
        let max_len = (chars.len() - i).min(6);
        let mut found = false;
        for len in (1..=max_len).rev() {
            let candidate: String = chars[i..i + len].iter().collect();
            if inputx_pinyin::is_valid_syllable(&candidate) {
                segments.push(Segment::Full(candidate));
                i += len;
                found = true;
                break;
            }
        }
        if !found {
            // No valid syllable — take one char as initial-only.
            segments.push(Segment::Initial(chars[i]));
            i += 1;
        }
    }

    segments
}

/// Check if a pinyin code (e.g., "guangyin sijian") matches the segment pattern.
/// Returns true if each segment's initial matches and full segments match exactly.
fn pattern_match(code: &str, segments: &[Segment]) -> bool {
    // Parse code into syllables via inputx segmenter.
    let code_syls = match inputx_pinyin::segment(code).into_iter().next() {
        Some(s) => s.syllables,
        None => return false,
    };

    if code_syls.len() < segments.len() {
        return false;
    }

    for (si, seg) in segments.iter().enumerate() {
        let syl = &code_syls[si];
        match seg {
            Segment::Full(s) => {
                if syl != s {
                    return false;
                }
            }
            Segment::Initial(c) => {
                if !syl.starts_with(*c) {
                    return false;
                }
            }
        }
    }

    true
}

/// Result from the lattice decoder.
pub struct LatticeResult {
    pub text: String,
    /// Frequency score from FST (normalized to 0.0-1.0).
    pub freq_score: f64,
    /// Match type for scoring weight.
    pub match_type: MatchType,
    /// 该词的完整拼音 code(前缀联想按"剩余未输入长度"衰减用)。
    pub pinyin: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MatchType {
    /// Complete full-pinyin match (every segment is Full).
    Full,
    /// Mixed: some Full, some Initial segments.
    Mixed,
    /// Pure initials (every segment is Initial).
    Initials,
    /// 全拼前缀联想:输入是词条拼音的前缀(如 naozh → naozhong 闹钟)。
    /// 覆盖 greedy_parse 切不开的"半截音节"输入(zh 不是合法音节,
    /// 旧 Mixed 路径段数膨胀导致永远匹配不上)。
    Prefix,
}

/// Lattice decoder backed by a FST dict + initials index.
pub struct LatticeDecoder {
    /// FST dict (for frequency scores and code lookup).
    fst: inputx_fsa::Dict<Vec<u8>>,
    /// Initials index: "gysj" → [(pinyin, word, freq), ...].
    /// Stores full pinyin code for pattern verification.
    initials_index: HashMap<String, Vec<(String, String, u64)>>,
    /// Path to the .fst file — for the cache filename + staleness check.
    fst_path: String,
    /// Size of the .fst on disk at load time; the cache header records it so a newer .fst
    /// (e.g. after a deb upgrade) invalidates the stale cache.
    fst_len: u64,
    /// Actual max word frequency seen when the index was built — carried in the
    /// cache v2 header. `0` = unknown (legacy v1 cache) → `FreqScale` auto-mode
    /// falls back to a fixed denominator. This is what keeps 501276 ≠ 500369:
    /// the log₂ normalization divides by the REAL top of the distribution.
    max_freq: f64,
}

impl LatticeDecoder {
    /// Build from an already-loaded FST. `fst_path` is the original .fst file path; the `.idx`
    /// cache lives in the USER data dir (`~/.desk-pilot/`), not next to the .fst — the .fst may
    /// sit in a read-only system dir (deb: `/usr/share/swift-ime/dict`).
    pub fn new(fst: inputx_fsa::Dict<Vec<u8>>, fst_path: &str) -> Self {
        let fst_len = std::fs::metadata(fst_path).map(|m| m.len()).unwrap_or(0);
        let mut decoder = LatticeDecoder {
            fst,
            initials_index: HashMap::new(),
            fst_path: fst_path.to_string(),
            fst_len,
            max_freq: 0.0,
        };
        decoder.build_initials_index();
        decoder
    }

    /// Candidate cache locations for a given .fst path, in preference order:
    /// 1. **next to the .fst** (`assets/dict/rime-ice.fst.idx`) — dev machines ship one in the
    ///    repo, so startup loads instantly instead of rebuilding the 29万-entry index (~46s).
    /// 2. **the `DATA` namespace** — dev: `apps/swift-ime/data/`, prod: `~/.desk-pilot/`
    ///    (the deb's `/usr/share/swift-ime/dict` is read-only, so a first run there
    ///    can't write beside the .fst and must fall back to the user data dir).
    ///
    /// Loading takes the first that exists; saving takes the first that's writable.
    fn cache_paths(fst_path: &str) -> Vec<std::path::PathBuf> {
        let mut v = Vec::with_capacity(2);
        v.push(std::path::PathBuf::from(format!("{fst_path}.idx")));
        let name = std::path::Path::new(fst_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| fst_path.to_string());
        // DATA 命名空间统一用户数据路径(dev/prod 由 Cargo.toml metadata 声明)。
        if let Some(p) = shared::loader!().resolve(&format!("DATA::{name}.idx")) {
            v.push(p);
        }
        v
    }

    /// Build the initials index from the FST, storing (pinyin, word, freq).
    /// Uses a .fst.idx cache file if available and fresh.
    fn build_initials_index(&mut self) {
        // Try cache first.
        if self.try_load_cache() {
            return;
        }
        let start = std::time::Instant::now();
        let mut imap: HashMap<String, Vec<(String, String, u64)>> = HashMap::new();
        let mut max_freq: u64 = 0;
        self.fst.prefix_for_each(b"", |code, item, value| {
            if let (Ok(pinyin), Ok(word)) = (
                std::str::from_utf8(code),
                std::str::from_utf8(item),
            ) {
                if !pinyin.is_empty() && !word.is_empty() {
                    max_freq = max_freq.max(value);
                    if let Some(seg) = inputx_pinyin::segment(pinyin).into_iter().next() {
                        let initials: String = seg.syllables.iter()
                            .filter_map(|s| s.chars().next())
                            .collect();
                        if initials.len() >= 2 {
                            imap.entry(initials).or_default()
                                .push((pinyin.to_string(), word.to_string(), value));
                        }
                    }
                }
            }
        });
        self.max_freq = max_freq as f64;
        for v in imap.values_mut() {
            v.sort_by_key(|(_, _, f)| std::cmp::Reverse(*f));
            v.dedup_by_key(|(_, w, _)| w.clone());
            v.truncate(128);
        }
        self.initials_index = imap;
        let elapsed = start.elapsed();
        eprintln!("[lattice] built initials index: {} entries in {}ms",
            self.initials_index.len(), elapsed.as_millis());
        // Save cache for next time.
        self.save_cache();
    }

    /// Cache format v1 (pre-2026-08-04): `# swift-ime idx v1 <fst_len>\n` — no
    /// max_freq; `FreqScale` auto-mode falls back to a fixed denominator.
    const CACHE_MAGIC_V1: &str = "# swift-ime idx v1";
    /// Cache format v2: `# swift-ime idx v2 <fst_len> <max_freq>\n` — carries the
    /// actual top weight so freq→score maps against the REAL distribution.
    const CACHE_MAGIC_V2: &str = "# swift-ime idx v2";

    fn try_load_cache(&mut self) -> bool {
        for cp in Self::cache_paths(&self.fst_path) {
            let Ok(data) = std::fs::read(&cp) else { continue };
            let header_end = match data.iter().position(|&b| b == b'\n') {
                Some(i) => i,
                None => continue, // no header line — legacy or corrupt
            };
            let header = String::from_utf8_lossy(&data[..header_end]);

            // v2: `# swift-ime idx v2 <fst_len> <max_freq>`. A different fst_len
            // ⇒ the .fst changed (deb upgrade) ⇒ stale.
            if let Some(rest) = header.strip_prefix(Self::CACHE_MAGIC_V2) {
                let mut parts = rest.split_whitespace();
                let Ok(fst_len) = parts.next().unwrap_or("").parse::<u64>() else {
                    continue;
                };
                if fst_len != self.fst_len {
                    continue; // stale — try the next candidate location
                }
                let max_freq: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                if self.parse_cache(&data[header_end + 1..]) {
                    self.max_freq = max_freq;
                    return true;
                }
                continue;
            }

            // v1 header: fresh only when the fst_len matches; no max_freq.
            if header.starts_with(Self::CACHE_MAGIC_V1) {
                let expected = format!("{} {}", Self::CACHE_MAGIC_V1, self.fst_len);
                if header != expected {
                    continue; // stale
                }
                if self.parse_cache(&data[header_end + 1..]) {
                    self.max_freq = 0.0; // unknown — FreqScale auto falls back
                    return true;
                }
                continue;
            }

            // Pre-header legacy format: shipped beside the .fst in the repo.
            // Freshness can't be verified, but it's committed together with the
            // .fst — trusted. Only accepted from the next-to-fst candidate, never
            // the user dir (a re-downloaded stale copy there must not win).
            let first = Self::cache_paths(&self.fst_path).into_iter().next()
                .is_some_and(|p| p == cp);
            if first && self.parse_cache(&data) {
                self.max_freq = 0.0; // unknown
                return true;
            }
        }
        false
    }

    /// Parse the binary blob after the header into `initials_index`.
    fn parse_cache(&mut self, data: &[u8]) -> bool {
        let mut pos = 0;
        let mut count = 0;
        while pos < data.len() {
            if pos + 1 > data.len() { break; }
            let kl = data[pos] as usize; pos += 1;
            if pos + kl > data.len() { break; }
            let key = String::from_utf8_lossy(&data[pos..pos + kl]).to_string();
            pos += kl;
            if pos + 2 > data.len() { break; }
            let n = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                if pos + 1 > data.len() { break; }
                let pl = data[pos] as usize; pos += 1;
                if pos + pl > data.len() { break; }
                let pinyin = String::from_utf8_lossy(&data[pos..pos + pl]).to_string();
                pos += pl;
                if pos + 1 > data.len() { break; }
                let wl = data[pos] as usize; pos += 1;
                if pos + wl > data.len() { break; }
                let word = String::from_utf8_lossy(&data[pos..pos + wl]).to_string();
                pos += wl;
                if pos + 8 > data.len() { break; }
                let freq = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                entries.push((pinyin, word, freq));
            }
            self.initials_index.insert(key, entries);
            count += 1;
        }
        if count > 0 {
            eprintln!("[lattice] loaded initials cache: {} entries", count);
        }
        count > 0
    }

    fn save_cache(&self) {
        // Build the blob once, then write to the first writable candidate (beside the .fst in
        // dev; the user dir when the system dir is read-only). Failing all is fine — next start
        // rebuilds.
        let mut buf = format!("{} {} {}\n", Self::CACHE_MAGIC_V2, self.fst_len, self.max_freq).into_bytes();
        for (key, entries) in &self.initials_index {
            buf.push(key.len() as u8);
            buf.extend_from_slice(key.as_bytes());
            buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
            for (pinyin, word, freq) in entries {
                buf.push(pinyin.len() as u8);
                buf.extend_from_slice(pinyin.as_bytes());
                buf.push(word.len() as u8);
                buf.extend_from_slice(word.as_bytes());
                buf.extend_from_slice(&freq.to_le_bytes());
            }
        }
        for cp in Self::cache_paths(&self.fst_path) {
            if let Some(parent) = cp.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&cp, &buf) {
                Ok(_) => {
                    eprintln!("[lattice] saved initials cache to {}", cp.display());
                    return;
                }
                Err(e) => eprintln!("[lattice] cache write to {} failed: {e}", cp.display()),
            }
        }
    }

    /// Convert a dict weight to the internal score, parameterized by `scale`.
    ///
    /// log₂ normalization against the distribution's REAL top:
    /// - `scale.max_weight > 0` — explicit fixed denominator (config override);
    /// - otherwise the actual max weight recorded at index build (cache v2) —
    ///   501276 (继续) and 500369 (机械) map to 1.0 and ~0.998 instead of tying;
    /// - legacy caches without a recorded max fall back to a fixed 600k.
    ///
    /// **线性重标**(不是 clamp)到 [scale.min_score, scale.max_score]
    /// (默认 0.25..0.90):score = min + (max-min) × log₂ 归一化比值。
    /// - 保持严格单调 —— 高频段(501276 继续 vs 164505 急须)不会像 clamp
    ///   那样贴顶同分、退化成稳定序;
    /// - 顶流封顶 max_score(0.90),给 recent/context 合成公式留加成空间
    ///   ((1-a)(a+b)/8+a 在 a→1 时失效)。
    pub fn freq_to_score(&self, scale: &crate::scoring::FreqScale, freq: u64) -> f64 {
        let w = freq.max(1) as f64;
        let max = if scale.max_weight > 0.0 {
            scale.max_weight
        } else if self.max_freq > 0.0 {
            self.max_freq
        } else {
            600_000.0 // legacy cache without recorded max
        };
        let ratio = (w + 1.0).log2() / (max + 1.0).log2();
        scale.min_score + (scale.max_score - scale.min_score) * ratio
    }

    /// Full-pinyin exact hit? (rime-ice contains `pinyin` → `word`) — used by
    /// learn_phrase to skip words that are already in the dictionary.
    pub fn has_word(&self, pinyin: &str, word: &str) -> bool {
        self.fst.get(pinyin.as_bytes())
            .iter()
            .any(|(item, _)| item == word.as_bytes())
    }

    /// 全拼命中的所有 (word, freq) 对 —— 供上下文感知的"前缀整词联想"
    /// (prev_pinyin + input 拼起来查整词)。
    pub fn words_for(&self, pinyin: &str) -> Vec<(String, u64)> {
        self.fst.get(pinyin.as_bytes())
            .iter()
            .map(|(item, value)| (String::from_utf8_lossy(item).into_owned(), *value))
            .collect()
    }

    /// 前缀联想收集池上限:FST 按字典序遍历、回调无法中断,宽前缀
    /// (如两字母声母)可能命中几千条。池必须足够大 —— 字典序 ≠ 频率序,
    /// 256 会把字典序靠后的高频词挡在门外(jixu/继续 排在几百个 jixi*
    /// 词之后)。收集后按词频排序取 top(截断在排序后),池大只影响
    /// 遍历耗时(~1024 条回调 <5ms)。
    const PREFIX_SCAN_CAP: usize = 1024;

    /// 全拼前缀联想:所有拼音以 `input` 为前缀的词条(词频降序)。
    /// `naozh` → `naozhong`(闹钟)—— 用户还在打字,联想出目标词。
    pub fn predict_prefix(&self, input: &str, max_results: usize) -> Vec<LatticeResult> {
        let mut results = Vec::new();
        self.fst.prefix_for_each(input.as_bytes(), |code, item, value| {
            if results.len() >= Self::PREFIX_SCAN_CAP {
                return;
            }
            if let (Ok(word), Ok(pinyin)) = (
                std::str::from_utf8(item),
                std::str::from_utf8(code),
            ) {
                if !word.is_empty() && !pinyin.is_empty() {
                    results.push(LatticeResult {
                        text: word.to_string(),
                        freq_score: value as f64,
                        match_type: MatchType::Prefix,
                        pinyin: pinyin.to_string(),
                    });
                }
            }
        });
        results.sort_by(|a, b| b.freq_score.partial_cmp(&a.freq_score).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(max_results);
        results
    }

    /// Main entry: predict candidates for any pinyin input.
    pub fn predict(&self, input: &str, max_results: usize) -> Vec<LatticeResult> {
        if input.is_empty() { return Vec::new(); }

        let segments = greedy_parse(input);
        if segments.is_empty() { return Vec::new(); }

        let all_full = segments.iter().all(|s| matches!(s, Segment::Full(_)));
        let all_initials = segments.iter().all(|s| matches!(s, Segment::Initial(_)));
        let match_type = if all_full { MatchType::Full }
            else if all_initials { MatchType::Initials }
            else { MatchType::Mixed };

        if all_full {
            let mut results = Vec::new();
            self.fst.get(input.as_bytes()).into_iter()
                .for_each(|(item, value)| {
                    if let Ok(word) = String::from_utf8(item) {
                        results.push(LatticeResult {
                            text: word, freq_score: value as f64, match_type: MatchType::Full,
                            pinyin: input.to_string(),
                        });
                    }
                });
            results.sort_by(|a, b| b.freq_score.partial_cmp(&a.freq_score).unwrap_or(std::cmp::Ordering::Equal));
            results.truncate(max_results);
            return results;
        }

        let initials: String = segments.iter().map(|s| s.initial()).collect();
        let candidates = match self.initials_index.get(&initials) {
            Some(v) => v.clone(), None => return Vec::new(),
        };

        let mut results = Vec::new();
        for (code, word, freq) in &candidates {
            if pattern_match(code, &segments) {
                results.push(LatticeResult {
                    text: word.clone(), freq_score: *freq as f64, match_type,
                    pinyin: code.clone(),
                });
            }
        }
        results.sort_by(|a, b| b.freq_score.partial_cmp(&a.freq_score).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(max_results);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greedy_parse_full() {
        let segs = greedy_parse("guangyinsijian");
        assert_eq!(segs.len(), 4);
        assert!(matches!(segs[0], Segment::Full(ref s) if s == "guang"));
    }

    #[test]
    fn greedy_parse_mixed() {
        let segs = greedy_parse("guangyinsj");
        assert_eq!(segs.len(), 4);
        assert!(matches!(segs[3], Segment::Initial('j')));
    }

    #[test]
    fn greedy_parse_complex_mixed() {
        let segs = greedy_parse("gyinsjian");
        assert_eq!(segs.len(), 4);
        assert!(matches!(segs[0], Segment::Initial('g')));
    }

    #[test]
    fn greedy_parse_pure_initials() {
        let segs = greedy_parse("gysj");
        assert_eq!(segs.len(), 4);
        assert!(matches!(segs[0], Segment::Initial('g')));
    }

    #[test]
    fn cache_paths_prefer_next_to_fst_then_data_namespace() {
        // Priority: beside the .fst (dev ships it in the repo → instant startup), then the
        // DATA 命名空间(dev: apps/swift-ime/data,prod: ~/.desk-pilot)。
        let paths = LatticeDecoder::cache_paths("/usr/share/swift-ime/dict/rime-ice.fst");
        assert_eq!(paths[0], std::path::PathBuf::from("/usr/share/swift-ime/dict/rime-ice.fst.idx"));
        assert_eq!(paths[1].file_name().map(|s| s.to_string_lossy().into_owned()),
            Some("rime-ice.fst.idx".into()), "DATA fallback name: {:?}", paths[1]);
        // dev 模式:第二候选落在仓库的 apps/swift-ime/data(不再硬编码 HOME)。
        assert!(paths[1].to_string_lossy().contains("apps/swift-ime/data"),
            "dev DATA namespace: {:?}", paths[1]);
    }

    #[test]
    fn freq_to_score_uses_scale_and_actual_max() {
        // Tiny FST with known weights — the index build records the REAL max
        // (501276). Auto-mode (max_weight=0) maps against it: the top word gets
        // 1.0 and a near-top word keeps a strictly smaller score (not tied).
        let path = format!("/tmp/swift-ime-lattice-scale-{}.fst", std::process::id());
        let mut b = inputx_fsa::DictBuilder::new();
        b.insert(b"jix", "机械".as_bytes(), 500_369);
        b.insert(b"jixu", "继续".as_bytes(), 501_276);
        b.insert(b"jixu", "急须".as_bytes(), 164_505);
        let dict = inputx_fsa::Dict::new(b.finish()).expect("dict");
        let dec = LatticeDecoder::new(dict, &path);
        let scale = crate::scoring::FreqScale { max_weight: 0.0, min_score: 0.25, max_score: 1.0 };
        // Auto + 线性重标:min + (max-min) × log₂ 比值。top word = max_score,
        // 近顶词保持严格更小(不贴顶同分)。
        assert!((dec.freq_to_score(&scale, 501_276) - 1.0).abs() < 1e-9, "top word = max_score");
        let s500 = dec.freq_to_score(&scale, 500_369);
        assert!(s500 < 1.0 && s500 > 0.99, "near-top keeps separation: {s500}");
        assert!(dec.freq_to_score(&scale, 164_505) < s500, "lower freq scores lower");
        // 显式分母 + 更紧的 [min,max]:重标保持单调,顶 = max_score、底 = min_score。
        let tight = crate::scoring::FreqScale { max_weight: 100_000.0, min_score: 0.25, max_score: 0.90 };
        assert!((dec.freq_to_score(&tight, 100_000) - 0.90).abs() < 1e-9, "top = max_score");
        // 10000/100000 → log₂ 比值 = 13.29/16.61 ≈ 0.800 → 0.25 + 0.65×0.800
        // ≈ 0.770(单调线性重标;clamp 时代此处为 0.80 贴段)。
        let s10k = dec.freq_to_score(&tight, 10_000);
        assert!((s10k - 0.770).abs() < 0.01, "rescaled mid: {s10k}");
        assert!(s10k < 0.90 && s10k > 0.25);
        // f=1:ratio = log₂2/log₂100001 ≈ 0.060 → 0.25 + 0.65×0.060 ≈ 0.289
        //(线性重标下最低频不精确落在 min_score,但严格大于它且单调)。
        assert!((dec.freq_to_score(&tight, 1) - 0.289).abs() < 0.01, "bottom near min");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}.idx"));
    }
}
