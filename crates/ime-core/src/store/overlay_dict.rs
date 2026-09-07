//! overlay_dict — L2:持久化的 Overlay 词典(round19 三级架构)。
//!
//! 与 L1([`super::memory::MemoryLayer`])同构的**两张独立表**:
//! 频率统计(`freq`:词 → 拼音/基础频率/累计次数)+ 时间统计
//! (`recent`:词 → 最近提交 ms)。L1 攒满 [`FLUSH_THRESHOLD`] 条整体
//! flush 进本层并清空;本层会话内只读(被 L1 覆盖时不改写,下一次
//! flush 才吸收新状态),全量快照落盘(SQLite `overlay_freq` /
//! `overlay_recent` 两表 —— 分表,不是一张表)。
//!
//! 作为**独立预测层**:启动冷加载,拼音家族以与 SeedDict 相同的查询
//! 方式调用(全拼精确命中 → (词, 有效频率)),自生词即使不在 seed
//! 也能出候选。查找优先级:L1 > L2 > L3(越热越权威)。

use super::memory::{FreqEntry, FREQ_ENHANCE_CAP, FREQ_ENHANCE_STEP};
use std::collections::HashMap;

/// L2:频率表 + 时间表。
#[derive(Default)]
pub struct OverlayDict {
    /// 频率统计:词 → (拼音, 基础频率, 累计次数)。
    freq: HashMap<String, FreqEntry>,
    /// 时间统计:词 → 最近提交 wall-clock ms。
    recent: HashMap<String, i64>,
    /// 生成号(round19):absorb/load 时递增 —— 家族侧据此把 L2 同步进
    /// lattice overlay 旁路(比对缓存代,避免每查询重灌)。
    generation: std::sync::atomic::AtomicU64,
}

impl OverlayDict {
    /// 当前生成号(家族侧 lattice 同步的变更检测依据)。
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn bump(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// L1 flush 的一次搬运量:频率表 + 时间表(分表,不是一张表)。
pub struct FlushBatch {
    pub freq: HashMap<String, FreqEntry>,
    pub recent: HashMap<String, i64>,
}

/// L2 全量快照(落盘 / 冷加载的行集;与 SQLite 两表一一对应)。
pub struct Snapshot {
    /// 频率表行:(word, pinyin, 基础频率, count)。
    pub freq: Vec<(String, String, u64, u32)>,
    /// 时间表行:(word, last_ms)。
    pub recent: Vec<(String, i64)>,
}

impl OverlayDict {
    /// L1 flush 吸收:同词合并(count 累加,频率取大者,拼音补齐),
    /// 时间取新。吸收后由调用方全量快照落盘。
    pub fn absorb(&mut self, batch: FlushBatch) {
        let FlushBatch { freq, recent } = batch;
        for (w, e) in freq {
            match self.freq.get_mut(&w) {
                Some(old) => {
                    old.count = old.count.saturating_add(e.count);
                    if e.frequency > old.frequency {
                        old.frequency = e.frequency;
                    }
                    if old.pinyin.is_empty() {
                        old.pinyin = e.pinyin;
                    }
                }
                None => {
                    self.freq.insert(w, e);
                }
            }
        }
        for (w, t) in recent {
            match self.recent.get(&w) {
                Some(old) if *old >= t => {}
                _ => {
                    self.recent.insert(w, t);
                }
            }
        }
        self.bump();
    }

    /// 独立预测层查询(与 SeedDict 同一方式):全拼精确命中 →
    /// (词, 有效频率)。有效频率 = 基础 + 频次增强(读取时派生)。
    pub fn lookup(&self, pinyin: &str) -> Vec<(String, u64)> {
        self.freq
            .iter()
            .filter(|(_, e)| !e.pinyin.is_empty() && e.pinyin == pinyin)
            .map(|(w, e)| (w.clone(), e.effective_frequency()))
            .collect()
    }

    /// 频率表条目(覆盖层取基础频率/映射对)。
    pub fn freq_entry(&self, word: &str) -> Option<&FreqEntry> {
        self.freq.get(word)
    }

    /// 时间表条目(三级穿透:tier 查询 L1 miss 时用)。
    pub fn recent_ms(&self, word: &str) -> Option<i64> {
        self.recent.get(word).copied()
    }

    /// 近期增益(与 L1 同一公式/同一常量,见
    /// [`crate::store::memory::MemoryLayer::recency_boost`];只读 ——
    /// 过期条目不惰性删,留待下次 flush 清理。**频率表不受影响**)。
    pub fn recency_boost(&self, word: &str, now_ms: i64) -> f64 {
        const T3D: i64 = 259_200_000;
        let Some(&last) = self.recent.get(word) else {
            return 0.0;
        };
        let age = now_ms - last;
        if age > T3D {
            return 0.0;
        }
        crate::store::memory::RECENCY_GAIN_MAX
            * f64::exp2(-(age as f64) / crate::store::memory::RECENCY_HALF_LIFE_MS as f64)
    }

    /// 冷加载(启动)。
    pub fn load(&mut self, freq: Vec<(String, String, u64, u32)>, recent: Vec<(String, i64)>) {
        for (w, p, f, c) in freq {
            self.freq.insert(
                w,
                FreqEntry {
                    pinyin: p,
                    count: c,
                    frequency: f,
                },
            );
        }
        for (w, t) in recent {
            self.recent.insert(w, t);
        }
        self.bump();
    }

    /// 全量快照(落盘用;基础频率,增强派生不落盘)。
    pub fn dump(&self) -> Snapshot {
        Snapshot {
            freq: self
                .freq
                .iter()
                .map(|(w, e)| (w.clone(), e.pinyin.clone(), e.frequency, e.count))
                .collect(),
            recent: self.recent.iter().map(|(w, t)| (w.clone(), *t)).collect(),
        }
    }

    /// 条数(诊断/测试)。
    pub fn len(&self) -> usize {
        self.freq.len()
    }

    pub fn is_empty(&self) -> bool {
        self.freq.is_empty()
    }

    /// 诊断:频次增强上界引用(保持常量被使用;上限语义与 L1 一致)。
    pub fn enhance_cap() -> u64 {
        let _ = (FREQ_ENHANCE_STEP, FREQ_ENHANCE_CAP);
        FREQ_ENHANCE_CAP
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryLayer;

    fn now() -> i64 {
        1_800_000_000_000
    }

    #[test]
    fn flush_absorb_merges_and_l1_clears() {
        let t = now();
        let mut l1 = MemoryLayer::default();
        l1.record_commit_weighted("你好", "nihao", t, Some(42_000));
        l1.record_commit("你好", "", t + 1);
        l1.record_commit("自生词", "zishengci", t);

        let mut l2 = OverlayDict::default();
        // 预置 L2 已有 你好(旧沉淀):count 合并、频率取大者。
        l2.load(
            vec![("你好".into(), "nihao".into(), 1_000, 4)],
            vec![("你好".into(), t - 86_400_000)],
        );
        let l1_freq: HashMap<String, FreqEntry> = HashMap::new();
        let _ = l1_freq;

        // 手工走 absorb(与 flush_into 等价)。
        let moved_freq: HashMap<String, FreqEntry> = HashMap::new();
        let _ = moved_freq;
        l1.flush_into(&mut l2);

        let e = l2.freq_entry("你好").unwrap();
        assert_eq!(e.count, 6, "L2.count 4 + L1.count 2");
        assert_eq!(e.frequency, 42_000, "频率取大者(继承 > 旧沉淀)");
        assert_eq!(l2.recent_ms("你好"), Some(t + 1), "时间取新");
        assert!(l1.is_empty(), "L1 清空(数据已搬走)");
        assert!(l1.recent_ms("你好").is_none());
    }

    #[test]
    fn lookup_exact_pinyin_with_enhancement() {
        let t = now();
        let mut l1 = MemoryLayer::default();
        l1.register_self_generated("李正明", "lizhengming");
        for _ in 0..3 {
            l1.record_commit("李正明", "lizhengming", t);
        }
        let mut l2 = OverlayDict::default();
        l1.flush_into(&mut l2);

        let hits = l2.lookup("lizhengming");
        assert_eq!(hits.len(), 1);
        let (w, f) = &hits[0];
        assert_eq!(w, "李正明");
        // 基础 100 + 3×10 增强 → 130。
        assert_eq!(*f, 130);
        assert!(l2.lookup("lizhengmin").is_empty(), "全拼精确,不做前缀");
    }

    #[test]
    fn recency_boost_reads_l2_recent() {
        let t = now();
        let mut l2 = OverlayDict::default();
        l2.load(
            vec![("刚用".into(), "gangyong".into(), 500, 1)],
            vec![("刚用".into(), t - 5_000)],
        );
        assert!((l2.recency_boost("刚用", t) - crate::store::memory::RECENCY_GAIN_MAX).abs() < 1e-4, "L2 直查(穿透测试在 wordbook 层)");
        assert_eq!(l2.recency_boost("不存在", t), 0.0);
    }

    #[test]
    fn dump_load_roundtrip() {
        let t = now();
        let mut l1 = MemoryLayer::default();
        l1.record_commit_weighted("词", "ci", t, Some(9_000));
        let mut l2 = OverlayDict::default();
        l1.flush_into(&mut l2);
        let snap = l2.dump();

        let mut l2b = OverlayDict::default();
        l2b.load(snap.freq, snap.recent);
        assert_eq!(l2b.freq_entry("词").unwrap().frequency, 9_000);
        assert_eq!(l2b.recent_ms("词"), Some(t));
    }
}
