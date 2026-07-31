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
    /// Path to the .fst file, used to derive the .idx cache path.
    fst_path: String,
}

impl LatticeDecoder {
    /// Build from an already-loaded FST. `fst_path` is the original .fst
    /// file path, used to derive the `.idx` cache location.
    pub fn new(fst: inputx_fsa::Dict<Vec<u8>>, fst_path: &str) -> Self {
        let mut decoder = LatticeDecoder {
            fst,
            initials_index: HashMap::new(),
            fst_path: fst_path.to_string(),
        };
        decoder.build_initials_index();
        decoder
    }

    /// Cache path: `{fst_path}.idx`.
    fn cache_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("{}.idx", self.fst_path))
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
        self.fst.prefix_for_each(b"", |code, item, value| {
            if let (Ok(pinyin), Ok(word)) = (
                std::str::from_utf8(code),
                std::str::from_utf8(item),
            ) {
                if !pinyin.is_empty() && !word.is_empty() {
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

    fn try_load_cache(&mut self) -> bool {
        let cp = self.cache_path();
        let Ok(data) = std::fs::read(&cp) else { return false };
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
        let cp = self.cache_path();
        let mut buf = Vec::new();
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
        let _ = std::fs::write(&cp, &buf);
    }

    /// Score from rime-ice weight — log₂ normalization.
    /// MAX_WEIGHT ≈ 100000 (rime-ice max weight).
    const MAX_WEIGHT: f64 = 100_000.0;

    /// Convert weight to 0.25-0.90 range.
    /// weight=100000 → 0.90, weight=10000 → 0.85, weight=100 → 0.43
    pub fn freq_to_score(freq: u64) -> f64 {
        let w = freq.max(1) as f64;
        let s = (w + 1.0).log2() / (Self::MAX_WEIGHT + 1.0).log2();
        s.clamp(0.25, 0.90)
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
}
