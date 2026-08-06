//! RecentStore — the pinyin family's **recent member**: 最近使用词的
//! 时间分档加权。
//!
//! 每次提交最终结果时记录该词(带 wall-clock 时间戳);该词再次出现在候选
//! 列表中时获得权重加持,幅度按"距上次使用的时间"分为五个**近期指数等级**:
//!
//! | 距上次使用 | 近期指数 b |
//! |------------|-----------|
//! | ≤ 10s      | 5         |
//! | ≤ 1h       | 4         |
//! | ≤ 5h       | 3         |
//! | ≤ 1d       | 2         |
//! | ≤ 3d       | 1         |
//! | > 3d       | 0 —— 条目移出 |
//!
//! 权重合成公式(在拼音家族的 Layer 1 应用):
//!
//! ```text
//! z = (1 - a) * (a + b) / 8 + a
//! ```
//!
//! - `a` = 候选词原本权重(词典/lattice 基础分,一般 0.7~0.9)
//! - `b` = 近期指数(1-5)
//! - `z` = 新权重
//!
//! 公式性质:增量与 (1-a) 成比例 —— 低权重词获得更大加成、高权重词增量
//! 趋零,因此 z 天然 < 1,不会把高频词顶满(旧做法直接加固定值再 min(1.0),
//! 常用词一用就顶到 1.000)。时间戳用 wall-clock 毫秒(unix epoch),可跨
//! 会话持久化(SQLite recency 表)。

use std::collections::HashMap;

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
}

impl RecentStore {
    pub fn new() -> Self {
        RecentStore { entries: HashMap::new() }
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

    /// 该词当前的近期指数(1-5;0 = 不在记录或超过 3d,无加成)。
    /// `now_ms` = wall-clock ms。超过 3d 的条目在查询时被移出(惰性淘汰)。
    pub fn tier(&mut self, word: &str, now_ms: i64) -> u32 {
        let Some(&last) = self.entries.get(word) else {
            return 0;
        };
        let age = now_ms - last;
        let t = if age <= T10S {
            5
        } else if age <= T1H {
            4
        } else if age <= T5H {
            3
        } else if age <= T1D {
            2
        } else if age <= T3D {
            1
        } else {
            // 超过 3d:移出(惰性淘汰),不再有加成。
            self.entries.remove(word);
            return 0;
        };
        t
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
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        1_800_000_000_000 // 固定基准,便于断言
    }

    #[test]
    fn tiered_index_by_age() {
        let mut store = RecentStore::new();
        let t = now();
        store.record("你", t - 5_000); // 5s 前
        store.record("好", t - 60_000); // 1min 前
        store.record("的", t - 2 * 3_600_000); // 2h 前
        store.record("中", t - 12 * 3_600_000); // 12h 前
        store.record("国", t - 2 * 86_400_000); // 2d 前
        assert_eq!(store.tier("你", t), 5, "≤10s → 等级 5");
        assert_eq!(store.tier("好", t), 4, "≤1h → 等级 4");
        assert_eq!(store.tier("的", t), 3, "≤5h → 等级 3");
        assert_eq!(store.tier("中", t), 2, "≤1d → 等级 2");
        assert_eq!(store.tier("国", t), 1, "≤3d → 等级 1");
    }

    #[test]
    fn expired_entries_evicted() {
        let mut store = RecentStore::new();
        let t = now();
        store.record("旧词", t - 4 * 86_400_000); // 4d 前 — 超窗
        store.record("新词", t - 1_000);
        assert_eq!(store.tier("旧词", t), 0, ">3d → 无加成");
        assert!(!store.entries.contains_key("旧词"), ">3d 条目被移出");
        assert_eq!(store.tier("新词", t), 5);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn missing_word_returns_zero() {
        let mut store = RecentStore::new();
        store.record("测试", now());
        assert_eq!(store.tier("不存在", now()), 0);
    }

    #[test]
    fn re_record_refreshes_timestamp() {
        let mut store = RecentStore::new();
        let t = now();
        store.record("词", t - 2 * 86_400_000); // 2d 前
        store.record("词", t - 1_000); // 刚刚又用了一次 → 刷新
        assert_eq!(store.tier("词", t), 5, "刷新后回到最高等级");
    }

    #[test]
    fn persistence_roundtrip_skips_expired() {
        let t = now();
        let mut a = RecentStore::new();
        a.record("有效", t - 1_000);
        a.record("过期", t - 4 * 86_400_000);
        let dump = a.dump();
        assert_eq!(dump.len(), 2, "dump 含全部(淘汰发生在查询时)");

        let mut b = RecentStore::new();
        b.load_bulk(dump, t);
        assert_eq!(b.len(), 1, "加载时丢弃过期条目");
        assert_eq!(b.tier("有效", t), 5);
    }
}
