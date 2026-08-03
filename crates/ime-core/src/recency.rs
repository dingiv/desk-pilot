//! RecencyStore — short-term memory of recently committed words.
//!
//! A fixed-capacity ring buffer that gives decaying position-based boosts
//! to candidates that match recently used words. The most recent word gets
//! the highest boost; older words contribute less.
//!
//! ## Scoring
//!
//! | Position            | Boost |
//! |---------------------|-------|
//! | most recent (pos=0) | 0.20  |
//! | pos=1               | 0.15  |
//! | pos=2               | 0.10  |
//! | pos=3–9             | 0.05  |
//! | pos=10–63           | 0.02  |
//!
//! All boosts are additive and capped by the caller at 1.0.

use std::collections::VecDeque;

/// Maximum number of recent words tracked.
const MAX_RECENT: usize = 64;

#[derive(Debug, Clone)]
pub struct RecencyStore {
    recency: VecDeque<String>,
}

impl RecencyStore {
    pub fn new() -> Self {
        RecencyStore { recency: VecDeque::with_capacity(MAX_RECENT) }
    }

    /// Number of words currently tracked.
    pub fn len(&self) -> usize {
        self.recency.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recency.is_empty()
    }

    /// Record a committed word. Evicts the oldest entry when at capacity.
    pub fn push(&mut self, word: &str) {
        if word.is_empty() { return; }
        // Don't store duplicates consecutively.
        if self.recency.front().map_or(false, |f| f == word) {
            return;
        }
        if self.recency.len() >= MAX_RECENT {
            self.recency.pop_back();
        }
        self.recency.push_front(word.to_string());
    }

    /// Calculate a position-based boost for a given word. Returns 0.0 if
    /// the word is not in the recency store.
    pub fn boost(&self, word: &str) -> f64 {
        for (i, w) in self.recency.iter().enumerate() {
            if w == word {
                return match i {
                    0 => 0.20,
                    1 => 0.15,
                    2 => 0.10,
                    3..=9 => 0.05,
                    _ => 0.02,
                };
            }
        }
        0.0
    }

    /// Bulk-load entries from persisted data (oldest first → newest last).
    /// Each `push` inserts at the front, so the last entry in the slice
    /// becomes the most recent (highest boost).
    pub fn load_bulk(&mut self, entries: &[String]) {
        for w in entries {
            self.push(w);
        }
    }

    /// Return all entries (most recent first) for persistence.
    pub fn dump(&self) -> Vec<String> {
        self.recency.iter().cloned().collect()
    }
}

impl Default for RecencyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_gets_highest_boost() {
        let mut store = RecencyStore::new();
        store.push("大陆");
        store.push("中国");
        // "中国" is most recent (pos=0), "大陆" is pos=1.
        assert!((store.boost("中国") - 0.20).abs() < 0.001);
        assert!((store.boost("大陆") - 0.15).abs() < 0.001);
    }

    #[test]
    fn missing_word_returns_zero() {
        let mut store = RecencyStore::new();
        store.push("测试");
        assert_eq!(store.boost("不存在"), 0.0);
    }

    #[test]
    fn consecutive_duplicate_not_stored() {
        let mut store = RecencyStore::new();
        store.push("你");
        store.push("你");
        store.push("你");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut store = RecencyStore::new();
        for i in 0..70 {
            store.push(&format!("w{i}"));
        }
        assert_eq!(store.len(), MAX_RECENT);
        // w69 should be most recent, w6 should be evicted.
        assert!((store.boost("w69") - 0.20).abs() < 0.001);
        assert_eq!(store.boost("w0"), 0.0); // evicted
    }

    #[test]
    fn empty_returns_zero() {
        let store = RecencyStore::new();
        assert_eq!(store.boost("anything"), 0.0);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn load_bulk_preserves_order() {
        let mut store = RecencyStore::new();
        store.load_bulk(&["a".into(), "b".into(), "c".into()]);
        // "c" loaded last = most recent.
        assert!((store.boost("c") - 0.20).abs() < 0.001);
        assert!((store.boost("b") - 0.15).abs() < 0.001);
        assert!((store.boost("a") - 0.10).abs() < 0.001);
    }
}
