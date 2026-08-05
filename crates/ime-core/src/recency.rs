//! RecencyStore — short-term memory of recently committed words.
//!
//! A fixed-capacity ring buffer that gives decaying position-based boosts
//! to candidates that match recently used words. The most recent word gets
//! the highest boost; older words contribute less.
//!
//! ## Scoring (configurable via [`crate::scoring::RecencyBoosts`])
//!
//! | Position            | Boost (default) |
//! |---------------------|-----------------|
//! | most recent (pos=0) | 0.20            |
//! | pos=1               | 0.15            |
//! | pos=2               | 0.10            |
//! | pos=3–9             | 0.05            |
//! | pos=10–63           | 0.02            |
//!
//! All boosts are additive and capped by the caller at 1.0.

use std::collections::VecDeque;

use crate::scoring::RecencyBoosts;


/// Maximum number of recent words tracked.
const MAX_RECENT: usize = 64;

#[derive(Debug, Clone)]
pub struct RecencyStore {
    recency: VecDeque<String>,
    /// Position→boost table (from config; defaults match the legacy values).
    boosts: RecencyBoosts,
}

impl RecencyStore {
    pub fn new(boosts: RecencyBoosts) -> Self {
        RecencyStore { recency: VecDeque::with_capacity(MAX_RECENT), boosts }
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
        let b = &self.boosts;
        for (i, w) in self.recency.iter().enumerate() {
            if w == word {
                return match i {
                    0 => b.pos0,
                    1 => b.pos1,
                    2 => b.pos2,
                    3..=9 => b.mid,
                    _ => b.far,
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
        Self::new(RecencyBoosts::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_gets_highest_boost() {
        let mut store = RecencyStore::new(RecencyBoosts::default());
        store.push("大陆");
        store.push("中国");
        // "中国" is most recent (pos=0), "大陆" is pos=1.
        assert!((store.boost("中国") - 0.20).abs() < 0.001);
        assert!((store.boost("大陆") - 0.15).abs() < 0.001);
    }

    #[test]
    fn missing_word_returns_zero() {
        let mut store = RecencyStore::new(RecencyBoosts::default());
        store.push("测试");
        assert_eq!(store.boost("不存在"), 0.0);
    }

    #[test]
    fn consecutive_duplicate_not_stored() {
        let mut store = RecencyStore::new(RecencyBoosts::default());
        store.push("你");
        store.push("你");
        store.push("你");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut store = RecencyStore::new(RecencyBoosts::default());
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
        let store = RecencyStore::new(RecencyBoosts::default());
        assert_eq!(store.boost("anything"), 0.0);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn load_bulk_preserves_order() {
        let mut store = RecencyStore::new(RecencyBoosts::default());
        store.load_bulk(&["a".into(), "b".into(), "c".into()]);
        // "c" loaded last = most recent.
        assert!((store.boost("c") - 0.20).abs() < 0.001);
        assert!((store.boost("b") - 0.15).abs() < 0.001);
        assert!((store.boost("a") - 0.10).abs() < 0.001);
    }

    #[test]
    fn custom_boosts_override_defaults() {
        // Config-driven boosts (swift-ime.yaml) replace the legacy table.
        let boosts = RecencyBoosts { pos0: 0.40, pos1: 0.30, pos2: 0.20, mid: 0.10, far: 0.05 };
        let mut store = RecencyStore::new(boosts);
        store.push("最新"); // 最后 push 的是最近 → pos=2
        store.push("次新");
        store.push("第三");
        assert!((store.boost("第三") - 0.40).abs() < 0.001, "newest gets pos0 boost");
        assert!((store.boost("次新") - 0.30).abs() < 0.001);
        assert!((store.boost("最新") - 0.20).abs() < 0.001);
    }
}
