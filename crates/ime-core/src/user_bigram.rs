//! UserBigram — lightweight user history model (fcitx5 HistoryBigram-style).
//!
//! Records `(prev_word, next_word)` co-occurrence counts. When the user
//! types a new word, candidates known to follow the previous word are
//! boosted. Persisted as JSON for session survival.

use std::collections::HashMap;

use crate::scoring::BigramTuning;

/// Key: (previous_word, next_word) → occurrence count.
type BigramKey = (String, String);

#[derive(Debug, Clone)]
pub struct UserBigram {
    counts: HashMap<BigramKey, u32>,
    /// Max count in the model — for score normalization.
    max_count: u32,
    /// Boost ceiling above 1.0 (from config; default 0.25 = max +25%).
    max_boost: f64,
}

impl Default for UserBigram {
    fn default() -> Self {
        Self::new()
    }
}

impl UserBigram {
    pub fn new() -> Self { UserBigram::with_tuning(BigramTuning::default()) }

    /// Construct with a configurable boost ceiling (`swift-ime.yaml`).
    pub fn with_tuning(tuning: BigramTuning) -> Self {
        UserBigram { counts: HashMap::new(), max_count: 0, max_boost: tuning.max_boost }
    }

    pub fn is_empty(&self) -> bool { self.counts.is_empty() }

    /// Record that `next` followed `prev` in user input.
    pub fn record(&mut self, prev: &str, next: &str) {
        if prev.is_empty() || next.is_empty() { return; }
        let key = (prev.to_string(), next.to_string());
        let c = self.counts.entry(key).or_insert(0);
        *c += 1;
        self.max_count = self.max_count.max(*c);
    }

    /// Boost score for a candidate if it frequently follows any context word.
    /// Returns a multiplier in [1.0, 1.0 + max_boost] based on bigram frequency.
    pub fn boost(&self, context_words: &[String], candidate: &str) -> f64 {
        if candidate.is_empty() || context_words.is_empty() { return 1.0; }
        let mut total: u32 = 0;
        for ctx in context_words {
            if let Some(c) = self.counts.get(&(ctx.to_string(), candidate.to_string())) {
                total += c;
            }
        }
        if total == 0 { return 1.0; }
        // Normalize: max boost (default 0.25 → +25%) for highest-frequency bigrams.
        let ratio = total as f64 / self.max_count.max(1) as f64;
        1.0 + ratio * self.max_boost
    }

    /// Export as JSON for persistence.
    pub fn export_json(&self) -> String {
        let mut pairs: Vec<String> = self.counts.iter()
            .map(|((p, n), c)| format!("[\"{p}\",\"{n}\",{c}]"))
            .collect();
        pairs.sort();
        format!("[{}]", pairs.join(","))
    }

    /// Import from JSON.
    pub fn import_json(&mut self, json: &str) {
        self.counts.clear();
        self.max_count = 0;
        // Parse [[prev, next, count], ...]
        if let Ok(arr) = serde_json::from_str::<Vec<(String, String, u32)>>(json) {
            for (prev, next, count) in arr {
                self.counts.insert((prev, next), count);
                self.max_count = self.max_count.max(count);
            }
        }
    }

    /// Bulk-load from a vec of (prev, next, count). Merges with existing data.
    /// Used on startup to warm the model from SQLite.
    pub fn load_bulk(&mut self, entries: Vec<(String, String, u32)>) {
        for (prev, next, count) in entries {
            let c = self.counts.entry((prev, next)).or_insert(0);
            *c = (*c).max(count); // take the max (SQLite is authoritative)
            self.max_count = self.max_count.max(*c);
        }
    }

    /// Number of bigram entries.
    pub fn len(&self) -> usize { self.counts.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_boost() {
        let mut m = UserBigram::new();
        m.record("大", "陆");
        m.record("大", "陆");
        m.record("大", "路");

        // 大陆 should get stronger boost than 路
        let boost_lu = m.boost(&["大".into()], "陆");
        let boost_lu2 = m.boost(&["大".into()], "路");
        assert!(boost_lu > boost_lu2, "大陆 boost {} should > 路 boost {}", boost_lu, boost_lu2);
    }

    #[test]
    fn empty_context_no_boost() {
        let m = UserBigram::new();
        assert!((m.boost(&[], "陆") - 1.0).abs() < 0.001);
    }

    #[test]
    fn roundtrip_json() {
        let mut m = UserBigram::new();
        m.record("大", "陆");
        m.record("大", "陆");
        let json = m.export_json();
        let mut m2 = UserBigram::new();
        m2.import_json(&json);
        assert!((m2.boost(&["大".into()], "陆") - 1.0) > 0.01);
    }
}
