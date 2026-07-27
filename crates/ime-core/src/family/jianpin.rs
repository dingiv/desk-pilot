//! JianpinFamily — initial-only pinyin matching (简拼).
//!
//! Matches candidates using only the first letter of each pinyin syllable.
//! E.g., "snsq" → 熟能生巧 (sheng neng sheng qiao → s-n-s-q).
//!
//! Implementation: builds a reverse index mapping initials strings to
//! (word, frequency) pairs by iterating the inputx-pinyin dictionary.

use super::{CandidateFamily, ScoredCandidate};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Pre-built initials index: initials (e.g. "snsq") → list of matching words.
type InitialsIndex = HashMap<String, Vec<(String, u64)>>;

pub struct JianpinFamily {
    engine: inputx_pinyin::PinyinEngine,
    index: OnceLock<InitialsIndex>,
    enabled: bool,
}

impl JianpinFamily {
    pub fn new() -> Self {
        JianpinFamily {
            engine: inputx_pinyin::PinyinEngine::with_fuzzy(
                inputx_pinyin::FuzzyConfig::permissive(),
            ),
            index: OnceLock::new(),
            enabled: true,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Build the initials reverse index (lazy, one-time).
    fn get_index(&self) -> &InitialsIndex {
        self.index.get_or_init(|| build_initials_index(&self.engine))
    }
}

impl Default for JianpinFamily {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateFamily for JianpinFamily {
    fn name(&self) -> &'static str {
        "jianpin"
    }

    fn priority(&self) -> u32 {
        85
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn top_n(&self) -> usize {
        4
    }

    fn predict(&self, input: &str) -> Vec<ScoredCandidate> {
        if input.is_empty() || input.len() < 2 {
            return Vec::new();
        }

        let input_lower = input.to_ascii_lowercase();
        let index = self.get_index();
        let mut out = Vec::new();

        // Exact initials match.
        if let Some(entries) = index.get(&input_lower) {
            // Find max frequency for normalization.
            let max_freq = entries.iter().map(|(_, f)| *f).max().unwrap_or(1);
            for (word, freq) in entries {
                let score = 0.5 + 0.5 * (*freq as f64 / max_freq as f64);
                out.push(ScoredCandidate {
                    text: word.clone(),
                    family: "jianpin",
                    raw_score: score.clamp(0.0, 1.0),
                });
            }
        }

        // Prefix match: find entries whose initials START with the input.
        for (initials, entries) in index.iter() {
            if initials.starts_with(&input_lower) && initials != &input_lower {
                let max_freq = entries.iter().map(|(_, f)| *f).max().unwrap_or(1);
                for (word, freq) in entries.iter().take(4) {
                    let score = 0.3 * (*freq as f64 / max_freq as f64);
                    out.push(ScoredCandidate {
                        text: word.clone(),
                        family: "jianpin",
                        raw_score: score.clamp(0.0, 1.0),
                    });
                }
            }
        }

        out.sort_by(|a, b| b.raw_score.partial_cmp(&a.raw_score).unwrap());
        out
    }
}

/// Extract initials from a raw (concatenated) pinyin string by
/// segmenting into syllables first.
/// E.g., "shengnengshengqiao" → "snsq".
fn initials_from_pinyin(raw: &str) -> String {
    let segs = inputx_pinyin::segment(raw);
    // Use the first segmentation (fewest syllables = longest-match preference).
    segs.first()
        .map(|seg| {
            seg.syllables.iter()
                .filter_map(|syl| syl.chars().next())
                .collect()
        })
        .unwrap_or_default()
}

/// Build the reverse index by scanning every valid pinyin syllable
/// and collecting multi-character word entries.
fn build_initials_index(engine: &inputx_pinyin::PinyinEngine) -> InitialsIndex {
    let dict = engine.dict();
    let mut index: InitialsIndex = HashMap::new();

    // Iterate all 403 valid pinyin syllables.
    for syl in inputx_pinyin::VALID_SYLLABLES {
        let results = dict.prefix(syl);
        for (pinyin, word) in results.iter().take(500) {
            let init = initials_from_pinyin(pinyin);
            if init.len() >= 2 && word.chars().count() >= 2 {
                index.entry(init)
                    .or_default()
                    .push((word.clone(), 1000));
            }
        }
    }

    // Deduplicate and sort by word length desc (longer = more specific).
    for entries in index.values_mut() {
        entries.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));
        entries.dedup_by(|a, b| a.0 == b.0);
    }

    tracing::info!(entries = index.len(), "jianpin index built");
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_extraction() {
        assert_eq!(initials_from_pinyin("shengnengshengqiao"), "snsq");
        assert_eq!(initials_from_pinyin("zhongguo"), "zg");
        assert_eq!(initials_from_pinyin("nihao"), "nh");
    }

    #[test]
    fn jianpin_predict_smoke() {
        let fam = JianpinFamily::new();
        // "nh" should match something
        let cands = fam.predict("nh");
        assert!(!cands.is_empty(), "nh should have candidates, index built?");
        eprintln!("nh candidates: {:?}", cands.iter().map(|c| &c.text).take(10).collect::<Vec<_>>());
    }

    #[test]
    fn jianpin_prefix() {
        let fam = JianpinFamily::new();
        let cands = fam.predict("sns");
        assert!(!cands.is_empty(), "sns prefix should have candidates");
    }

    #[test]
    fn too_short_input_returns_empty() {
        let fam = JianpinFamily::new();
        assert!(fam.predict("s").is_empty());
    }

    #[test]
    fn initials_from_pinyin_no_spaces() {
        // Now uses segment() so "nihao" → "ni"+"hao" → "nh"
        assert_eq!(initials_from_pinyin("nihao"), "nh");
    }
}
