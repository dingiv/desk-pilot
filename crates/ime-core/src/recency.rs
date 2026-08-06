//! RecentStore — the pinyin family's **recent member**: 最近使用词的
//! 时间分档加权。
//!
//! 每次提交最终结果时记录该词(带 wall-clock 时间戳);该词再次出现在候选
//! 列表中时获得权重加持,幅度按"距上次使用的时间"分档衰减:
//!
//! | 距上次使用 | Boost (默认,可配) |
//! |------------|--------------------|
//! | ≤ 10s      | 0.20              |
//! | ≤ 1h       | 0.15              |
//! | ≤ 5h       | 0.10              |
//! | ≤ 1d       | 0.05              |
//! | ≤ 3d       | 0.02              |
//! | > 3d       | 0.00 —— 条目从记录中移出 |
//!
//! 与旧的位置衰减(最近 64 词的环形)不同:这里以时间为准,3 天窗口内的
//! 所有使用词都参与;超出窗口的惰性淘汰(查询/提交时顺带清理)。时间戳用
//! wall-clock 毫秒(unix epoch),可跨会话持久化(SQLite recency 表)。

use std::collections::HashMap;

use crate::scoring::RecencyBoosts;

/// 档位边界(wall-clock 毫秒):10s / 1h / 5h / 1d / 3d。
const T10S: i64 = 10_000;
const T1H: i64 = 3_600_000;
const T5H: i64 = 18_000_000;
const T1D: i64 = 86_400_000;
const T3D: i64 = 259_200_000;

/// 记录上限(3 天窗口内使用词的自然上限;保险起见截断)。
const MAX_ENTRIES: usize = 512;

#[derive(Debug, Clone)]
pub struct RecentStore {
    /// word → 上次使用的 wall-clock ms (unix epoch)。
    entries: HashMap<String, i64>,
    /// 分档数值表(swift-ime.yaml → weights.recency)。
    boosts: RecencyBoosts,
}

impl RecentStore {
    pub fn new(boosts: RecencyBoosts) -> Self {
        RecentStore { entries: HashMap::new(), boosts }
    }

    /// 记录一次使用(word 在提交路径被选中)。`now_ms` = wall-clock ms。
    pub fn record(&mut self, word: &str, now_ms: i64) {
        if word.is_empty() {
            return;
        }
        self.entries.insert(word.to_string(), now_ms);
        // 保险:3 天窗口内词数超上限时,淘汰最旧的。
        if self.entries.len() > MAX_ENTRIES {
            let mut oldest: Vec<(String, i64)> = self.entries.drain().collect();
            oldest.sort_by_key(|(_, t)| *t);
            oldest.truncate(MAX_ENTRIES);
            self.entries = oldest.into_iter().collect();
        }
    }

    /// 该词当前应得的加持(0.0 = 不在记录或超过 3d)。`now_ms` = wall-clock ms。
    pub fn boost(&mut self, word: &str, now_ms: i64) -> f64 {
        let Some(&last) = self.entries.get(word) else {
            return 0.0;
        };
        let age = now_ms - last;
        let b = &self.boosts;
        let score = if age <= T10S {
            b.within_10s
        } else if age <= T1H {
            b.within_1h
        } else if age <= T5H {
            b.within_5h
        } else if age <= T1D {
            b.within_1d
        } else if age <= T3D {
            b.within_3d
        } else {
            // 超过 3d:移出(惰性淘汰),不再有加成。
            self.entries.remove(word);
            return 0.0;
        };
        score
    }

    /// 当前记录条数(诊断/测试)。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 全量导出 (word, last_used_ms) 用于持久化。
    pub fn dump(&self) -> Vec<(String, i64)> {
        self.entries.iter().map(|(w, t)| (w.clone(), *t)).collect()
    }

    /// 从持久化数据恢复。
    pub fn load_bulk(&mut self, entries: Vec<(String, i64)>, now_ms: i64) {
        for (w, t) in entries {
            // 过期条目(>3d)直接丢弃,不进入内存。
            if now_ms - t <= T3D {
                self.entries.insert(w, t);
            }
        }
    }
}

impl Default for RecentStore {
    fn default() -> Self {
        Self::new(RecencyBoosts::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        1_800_000_000_000 // 固定基准,便于断言
    }

    #[test]
    fn tiered_boost_by_age() {
        let mut store = RecentStore::new(RecencyBoosts::default());
        let t = now();
        store.record("你", t - 5_000); // 5s 前
        store.record("好", t - 60_000); // 1min 前
        store.record("的", t - 2 * 3_600_000); // 2h 前
        store.record("中", t - 12 * 3_600_000); // 12h 前
        store.record("国", t - 2 * 86_400_000); // 2d 前
        assert!((store.boost("你", t) - 0.20).abs() < 1e-9, "≤10s → 0.20");
        assert!((store.boost("好", t) - 0.15).abs() < 1e-9, "≤1h → 0.15");
        assert!((store.boost("的", t) - 0.10).abs() < 1e-9, "≤5h → 0.10");
        assert!((store.boost("中", t) - 0.05).abs() < 1e-9, "≤1d → 0.05");
        assert!((store.boost("国", t) - 0.02).abs() < 1e-9, "≤3d → 0.02");
    }

    #[test]
    fn expired_entries_evicted() {
        let mut store = RecentStore::new(RecencyBoosts::default());
        let t = now();
        store.record("旧词", t - 4 * 86_400_000); // 4d 前 — 超窗
        store.record("新词", t - 1_000);
        assert_eq!(store.boost("旧词", t), 0.0, ">3d → 无加成");
        assert!(!store.entries.contains_key("旧词"), ">3d 条目被移出");
        assert!((store.boost("新词", t) - 0.20).abs() < 1e-9);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn missing_word_returns_zero() {
        let mut store = RecentStore::new(RecencyBoosts::default());
        store.record("测试", now());
        assert_eq!(store.boost("不存在", now()), 0.0);
    }

    #[test]
    fn re_record_refreshes_timestamp() {
        let mut store = RecentStore::new(RecencyBoosts::default());
        let t = now();
        store.record("词", t - 2 * 86_400_000); // 2d 前
        store.record("词", t - 1_000); // 刚刚又用了一次 → 刷新
        assert!((store.boost("词", t) - 0.20).abs() < 1e-9, "刷新后回到 10s 档");
    }

    #[test]
    fn custom_boosts_override_defaults() {
        // 配置驱动的分档数值(swift-ime.yaml)。
        let boosts = RecencyBoosts {
            within_10s: 0.40, within_1h: 0.30, within_5h: 0.20,
            within_1d: 0.10, within_3d: 0.05,
        };
        let mut store = RecentStore::new(boosts);
        store.record("词", now() - 1_000);
        assert!((store.boost("词", now()) - 0.40).abs() < 1e-9);
    }

    #[test]
    fn persistence_roundtrip_skips_expired() {
        let t = now();
        let mut a = RecentStore::new(RecencyBoosts::default());
        a.record("有效", t - 1_000);
        a.record("过期", t - 4 * 86_400_000);
        let dump = a.dump();
        assert_eq!(dump.len(), 2, "dump 含全部(淘汰发生在查询时)");

        let mut b = RecentStore::new(RecencyBoosts::default());
        b.load_bulk(dump, t);
        assert_eq!(b.len(), 1, "加载时丢弃过期条目");
        assert!((b.boost("有效", t) - 0.20).abs() < 1e-9);
    }
}
