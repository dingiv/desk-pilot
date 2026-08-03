//! EnglishFamily — English word prediction via prefix matching.
//!
//! Activated for any ascii-lowercase input. Matches against a frequency-
//! weighted word list. The default embedded list has ~900 common English
//! words; a larger list can be loaded from a TSV file (`word\tfreq`).
//!
//! Scoring: `freq_to_score(freq)` — log₂ normalization, same formula as
//! the pinyin LatticeDecoder. Exact matches score 1.0, prefix matches are
//! scaled by length ratio.

use super::{CandidateFamily, ScoredCandidate};

/// Frequency-to-score: tiered normalization in [0.25, 0.90].
/// The English word list uses a 1-10000 range (not 1M like rime-ice).
fn freq_to_score(freq: u32) -> f64 {
    match freq {
        0 => 0.25,
        f if f >= 5000 => 0.90,
        f if f >= 1000 => 0.70,
        f if f >= 100  => 0.50,
        f if f >= 10   => 0.35,
        _              => 0.25,
    }
}

pub struct EnglishFamily {
    enabled: bool,
    /// Word list: (word, frequency). Sorted by word for binary search.
    words: Vec<(String, u32)>,
}

impl EnglishFamily {
    /// Create with an empty word list (call `load_dict_bytes` or `load_dict` to fill).
    pub fn new() -> Self {
        EnglishFamily { enabled: true, words: Vec::new() }
    }

    /// Create with the default embedded English word list.
    pub fn with_default_dict() -> Self {
        let mut fam = Self::new();
        let count = fam.load_tsv_bytes(Self::EMBEDDED_EN_DICT);
        if count > 0 {
            tracing::info!(count, "english: loaded embedded dictionary");
        }
        fam
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    // ── Dictionary loading ─────────────────────────────────────────────

    /// Load words from TSV bytes (`word\tfreq`). Deduplicates by keeping
    /// the highest frequency for each word.
    fn load_tsv_bytes(&mut self, data: &[u8]) -> usize {
        let mut map: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        if let Ok(s) = std::str::from_utf8(data) {
            for line in s.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') { continue; }
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() < 2 { continue; }
                let word = parts[0].to_ascii_lowercase();
                if word.is_empty() { continue; }
                let freq: u32 = parts[1].parse().unwrap_or(100);
                let entry = map.entry(word).or_insert(0);
                if freq > *entry { *entry = freq; }
            }
        }
        let count = map.len();
        self.words = map.into_iter().collect();
        self.words.sort_by(|a, b| a.0.cmp(&b.0));
        count
    }

    /// Embedded default English dictionary (TSV format), compiled into the binary.
    const EMBEDDED_EN_DICT: &[u8] =
        include_bytes!("../../../../apps/swift-ime/assets/dict/en_words.tsv");
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
        75
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn top_n(&self) -> usize {
        4
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() || !input.chars().all(|c| c.is_ascii_lowercase()) {
            return Vec::new();
        }

        let input_lower = input.to_ascii_lowercase();
        let mut out = Vec::new();

        // Binary search to find the range of words starting with `input_lower`.
        let start = self.words.binary_search_by(|(w, _)| {
            if w.as_str() < input_lower.as_str() { std::cmp::Ordering::Less }
            else { std::cmp::Ordering::Greater }
        }).unwrap_err();

        for (word, freq) in self.words[start..].iter() {
            if !word.starts_with(&input_lower) { break; }

            if *word == input_lower {
                // Exact match: score 1.0.
                out.push(ScoredCandidate {
                    text: word.clone(),
                    family: "english", source: "exact",
                    raw_score: 1.0,
                });
            } else {
                // Prefix match: frequency-based score × length ratio.
                let freq_score = freq_to_score(*freq);
                let len_ratio = input_lower.len() as f64 / word.len() as f64;
                let score = (freq_score * len_ratio).clamp(0.25, 0.95);
                out.push(ScoredCandidate {
                    text: word.clone(),
                    family: "english", source: "prefix",
                    raw_score: score,
                });
            }
        }

        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out.truncate(16);
        out
    }

    fn load_dict_bytes(&self, _data: &[u8]) -> usize {
        // For now, dict loading is done at construction time via with_default_dict().
        // External dict loading can be added later via load_dict().
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_black() {
        let fam = EnglishFamily::with_default_dict();
        let cands = fam.predict("black");
        assert!(cands.iter().any(|c| c.text == "black" && (c.raw_score - 1.0).abs() < 0.001));
    }

    #[test]
    fn prefix_blac() {
        let fam = EnglishFamily::with_default_dict();
        let cands = fam.predict("blac");
        assert!(cands.iter().any(|c| c.text == "black"));
        // "black" should be among top results.
        assert!(!cands.is_empty());
    }

    #[test]
    fn prefix_hel() {
        let fam = EnglishFamily::with_default_dict();
        let cands = fam.predict("hel");
        // Extended dict should include "help", "hello" wasn't in the old 200.
        assert!(!cands.is_empty());
        let words: Vec<&str> = cands.iter().map(|c| c.text.as_str()).collect();
        eprintln!("hel → {words:?}");
    }

    #[test]
    fn empty_returns_nothing() {
        let fam = EnglishFamily::with_default_dict();
        assert!(fam.predict("").is_empty());
    }

    #[test]
    fn no_match_garbage() {
        let fam = EnglishFamily::with_default_dict();
        let cands = fam.predict("zzzzz");
        assert!(cands.is_empty());
    }

    #[test]
    fn common_words_found() {
        let fam = EnglishFamily::with_default_dict();
        // These common words should be in the extended dict.
        for word in &["hello", "world", "python", "rust", "code", "data", "server"] {
            let cands = fam.predict(word);
            assert!(cands.iter().any(|c| c.text == *word),
                "{word} should be in english dict, got: {:?}",
                cands.iter().map(|c| &c.text).collect::<Vec<_>>());
        }
    }
}
