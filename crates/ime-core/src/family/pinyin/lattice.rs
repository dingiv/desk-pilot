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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MatchType {
    /// Complete full-pinyin match (every segment is Full).
    Full,
    /// Mixed: some Full, some Initial segments.
    Mixed,
    /// Pure initials (every segment is Initial).
    Initials,
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
    /// 2. **`~/.desk-pilot/`** — the deb's `/usr/share/swift-ime/dict` is read-only, so a first
    ///    run there can't write beside the .fst and must fall back to the user dir.
    /// Loading takes the first that exists; saving takes the first that's writable.
    fn cache_paths(fst_path: &str) -> Vec<std::path::PathBuf> {
        let mut v = Vec::with_capacity(2);
        v.push(std::path::PathBuf::from(format!("{fst_path}.idx")));
        let name = std::path::Path::new(fst_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| fst_path.to_string());
        v.push(std::path::Path::new(&std::env::var("HOME").unwrap_or_default())
            .join(".desk-pilot")
            .join(format!("{name}.idx")));
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
    /// clamp to [scale.min_score, scale.max_score] (defaults 0.25..1.0).
    pub fn freq_to_score(&self, scale: &crate::scoring::FreqScale, freq: u64) -> f64 {
        let w = freq.max(1) as f64;
        let max = if scale.max_weight > 0.0 {
            scale.max_weight
        } else if self.max_freq > 0.0 {
            self.max_freq
        } else {
            600_000.0 // legacy cache without recorded max
        };
        let s = (w + 1.0).log2() / (max + 1.0).log2();
        s.clamp(scale.min_score, scale.max_score)
    }

    /// Full-pinyin exact hit? (rime-ice contains `pinyin` → `word`) — used by
    /// learn_phrase to skip words that are already in the dictionary.
    pub fn has_word(&self, pinyin: &str, word: &str) -> bool {
        self.fst.get(pinyin.as_bytes())
            .iter()
            .any(|(item, _)| item == word.as_bytes())
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
    fn cache_paths_prefer_next_to_fst_then_user_dir() {
        // Priority: beside the .fst (dev ships it in the repo → instant startup), then the
        // user dir (deb system dir is read-only → first run falls back there).
        let paths = LatticeDecoder::cache_paths("/usr/share/swift-ime/dict/rime-ice.fst");
        assert_eq!(paths[0], std::path::PathBuf::from("/usr/share/swift-ime/dict/rime-ice.fst.idx"));
        assert_eq!(paths[1], std::path::PathBuf::from(format!("{}/.desk-pilot/rime-ice.fst.idx", std::env::var("HOME").unwrap())));
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
        // Auto: the recorded max is the top of the scale.
        assert!((dec.freq_to_score(&scale, 501_276) - 1.0).abs() < 1e-9, "top word = max_score");
        let s500 = dec.freq_to_score(&scale, 500_369);
        assert!(s500 < 1.0 && s500 > 0.99, "near-top keeps separation: {s500}");
        assert!(dec.freq_to_score(&scale, 164_505) < s500, "lower freq scores lower");
        // Explicit fixed denominator + tighter clamp override the recorded max.
        let tight = crate::scoring::FreqScale { max_weight: 100_000.0, min_score: 0.25, max_score: 0.90 };
        assert!((dec.freq_to_score(&tight, 100_000) - 0.90).abs() < 1e-9, "max_score cap applies");
        // 10000/100000 → log₂ ratio ≈ 0.80 < 0.90 cap (50000 would hit the cap).
        assert!((dec.freq_to_score(&tight, 10_000) - 0.80).abs() < 0.01);
        assert!((dec.freq_to_score(&tight, 1) - 0.25).abs() < 1e-9, "min_score floor applies");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}.idx"));
    }
}
