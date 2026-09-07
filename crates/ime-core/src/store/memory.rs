//! memory — OverlayData:L1 工作集(round16 建,round19 三级架构)。
//!
//! 三级:本层(L1,内存实时,小而热)→ L2 OverlayDict(持久化沉淀,
//! [`super::overlay_dict::OverlayDict`])→ L3 SeedDict(不可变)。
//! 本层攒满 [`FLUSH_THRESHOLD`] 条即整体 flush 进 L2 并清空(数据已搬走)。
//!
//! **频率统计与时间统计分表**(round19 指令):内部是两张独立的 map ——
//! `freq`(词 → 拼音/基础频率/累计次数)与 `recent`(词 → 最近提交
//! 时间)。它们是不同的表,不是一张表的几列:flush 时分别沉淀到 L2 的
//! 两张表,生命周期各自独立(时间表可三级穿透供 tier 查询)。
//!
//! 用户提交任何最终词后,后处理模块在此登记映射对。区别于从 dict 文件
//! 导入的静态词典,overlay 完全由使用行为生长:
//!
//! - **recency 归一**:时间分档近期指数(5 级,3 天窗口惰性淘汰);
//! - **自生词入册**:后处理学习路径在此登记(count = 0,默认频率);
//! - **频次增强**:提交次数与最近度共同决定加成档位。
//!
//! 所有权:随 `WordBook` 归持久化模块;家族、后处理经 `wordbook.memory`
//! 共享同一份(Mutex 内部可变)。

use std::collections::HashMap;

/// 自生词默认基础频率(与拼音家族 merged 覆盖层共用常量)。
pub const SELF_GEN_FREQUENCY: u64 = 100;
/// 频次增强步长:每累计 1 次提交,有效频率在基础频率上加这么多。
/// 加性小步长 —— 常用种子词(频次数千)相对漂移可忽略(排序近似稳定),
/// 自生词(基础 100)随使用稳步爬升。
pub const FREQ_ENHANCE_STEP: u64 = 10;
/// 频次增强上限(防止无界增长;约等于一个中等词频量级)。
pub const FREQ_ENHANCE_CAP: u64 = 5_000;
/// flush 阈值(round19):L1 频率表攒满即整体搬入 L2 并清空。
pub const FLUSH_THRESHOLD: usize = 128;

/// 频率统计表的一行:提交映射对 + 统计 + 基础频率。
/// (时间统计独立在 [`MemoryLayer::recent`] 表 —— round19 分表指令。)
#[derive(Debug, Clone, PartialEq)]
pub struct FreqEntry {
    /// 提交时的拼音(映射对的另一半;英文词为空)。
    pub pinyin: String,
    /// 累计提交次数(自生词登记时为 0,由提交增长)。
    pub count: u32,
    /// 词典域**基础频率**:种子词继承 SeedDict 原始频率;自生词取固定
    /// 默认([`SELF_GEN_FREQUENCY`])。0 = 尚无(旧迁移种子),覆盖算法
    /// 不作用于 0。注意:这不是运行时 0..1 的权重浮点 —— 与 FST value
    /// 同量纲,经 `freq_to_score` 线性重标后才参与排序;最终预测顺序由
    /// 运行时权重值决定。
    pub frequency: u64,
}

impl FreqEntry {
    /// **有效频率** = 基础频率 + 频次增强(随提交次数线性增长,封顶)。
    /// MergedDict 覆盖以此换算运行时权重;存储存基础值(稳定、无写放大),
    /// 增强在读取时派生。
    pub fn effective_frequency(&self) -> u64 {
        self.frequency
            .saturating_add((self.count as u64 * FREQ_ENHANCE_STEP).min(FREQ_ENHANCE_CAP))
    }
}

/// 衰减窗口(wall-clock 毫秒):超 3 天的近期加成归零(惰性移出)。
const T3D: i64 = 259_200_000;

/// 连续衰减半衰期(round20):b(t) = 5 × 2^(−t/HALF_LIFE)。取 18h
/// 对齐旧五档阶梯的中段锚点(5h≈4.1、1d≈2.2、3d≈0.3),但档内不再
/// 同分 —— 时间维度连续生效,两词只差几分钟也有可分辨的先后。
pub const RECENCY_HALF_LIFE_MS: i64 = 64_800_000;

/// 近期指数上颙(b 空间的最大值,与旧阶梯档位上限一致)。
pub const RECENCY_MAX: f64 = 5.0;

/// 记录上限(3 天窗口内使用词的自然上限;保险起见截断)。
const MAX_ENTRIES: usize = 512;

/// L1 工作集:频率统计表 + 时间统计表(独立,不是一张表)。
#[derive(Default)]
pub struct MemoryLayer {
    /// 频率统计:词 → (拼音, 基础频率, 累计次数)。
    freq: HashMap<String, FreqEntry>,
    /// 时间统计:词 → 最近提交 wall-clock ms。
    recent: HashMap<String, i64>,
}

impl MemoryLayer {
    /// 后处理提交登记:频率表计数 +1(并设定映射对/继承频率),
    /// 时间表独立盖章。
    pub fn record_commit(&mut self, word: &str, pinyin: &str, now_ms: i64) {
        self.record_commit_weighted(word, pinyin, now_ms, None);
    }

    /// 带频率版提交登记:`frequency = Some(f)` 设定 overlay 基础频率
    /// (种子词继承 SeedDict 频率 / 自生词默认);`None` 保留既有频率。
    pub fn record_commit_weighted(
        &mut self,
        word: &str,
        pinyin: &str,
        now_ms: i64,
        frequency: Option<u64>,
    ) {
        if word.is_empty() {
            return;
        }
        // 频率表:计数 + 继承频率 + 映射对。
        let e = self
            .freq
            .entry(word.to_string())
            .or_insert_with(|| FreqEntry {
                pinyin: String::new(),
                count: 0,
                frequency: 0,
            });
        if !pinyin.is_empty() {
            e.pinyin = pinyin.to_string();
        }
        if let Some(f) = frequency {
            e.frequency = f;
        }
        e.count = e.count.saturating_add(1);
        // 时间表:独立一行(分表,不与频率统计混存)。
        self.recent.insert(word.to_string(), now_ms);
        self.evict_overflow();
    }

    /// 自生词登记(后处理学习路径):入册但不计提交(count = 0),
    /// 赋固定默认频率(Wordbook 同步记录词 + 拼音 + 词频)。
    /// 已有条目只补拼音,不动计数与既有频率。
    pub fn register_self_generated(&mut self, word: &str, pinyin: &str) {
        if word.is_empty() {
            return;
        }
        let e = self
            .freq
            .entry(word.to_string())
            .or_insert_with(|| FreqEntry {
                pinyin: String::new(),
                count: 0,
                frequency: SELF_GEN_FREQUENCY,
            });
        if !pinyin.is_empty() {
            e.pinyin = pinyin.to_string();
        }
    }

    /// 近期指数(连续, 0.0..=5.0;0 = 不在时间表或超 3 天)。
    /// round20 起为**连续指数衰减**:`b(t) = 5 × 2^(−t/半衰期)`
    /// (半衰期 [`RECENCY_HALF_LIFE_MS`] = 18h),取代旧五档阶梯
    /// (10s/1h/5h/1d/3d 同档同分,两词只差几分钟也无法区分)。
    /// **提交次数参与加成**:`(count/3).min(1)` 连续逼近(取代旧
    /// ≥3 次的 +1 阶跃),封顶 [`RECENCY_MAX`];超 3 天惰性移出(消减)。
    /// 计数查频率表 —— 分表但联合判档。
    pub fn tier(&mut self, word: &str, now_ms: i64) -> f64 {
        let Some(&last) = self.recent.get(word) else {
            return 0.0;
        };
        let age = now_ms - last;
        if age > T3D {
            // 超过 3d:移出(惰性淘汰),不再有加成。
            self.recent.remove(word);
            return 0.0;
        }
        // 连续衰减:刚提交 b=5,每过半衰期减半。
        let b_time = RECENCY_MAX * 0.5f64.powf(age as f64 / RECENCY_HALF_LIFE_MS as f64);
        // 频次增强:count→3 线性爬升到 +1(旧版 ≥3 阶跃的连续化)。
        let count = self.freq.get(word).map(|e| e.count).unwrap_or(0);
        let bonus = (count as f64 / 3.0).min(1.0);
        (b_time + bonus).min(RECENCY_MAX)
    }

    /// 频率表条目(只读;诊断 / 覆盖层)。
    pub fn freq_entry(&self, word: &str) -> Option<&FreqEntry> {
        self.freq.get(word)
    }

    /// 时间表条目(只读;诊断 / flush 合并 / 三级穿透)。
    pub fn recent_ms(&self, word: &str) -> Option<i64> {
        self.recent.get(word).copied()
    }

    /// 频率表条数(flush 阈值的计量单位)。
    pub fn len(&self) -> usize {
        self.freq.len()
    }

    pub fn is_empty(&self) -> bool {
        self.freq.is_empty()
    }

    /// 按拼音前缀查频率表(预测源接口)。
    pub fn lookup_pinyin<'a>(&'a self, prefix: &str) -> Vec<(&'a str, &'a FreqEntry)> {
        self.freq
            .iter()
            .filter(|(_, e)| !e.pinyin.is_empty() && e.pinyin.starts_with(prefix))
            .map(|(w, e)| (w.as_str(), e))
            .collect()
    }

    /// 整体 flush(round19):频率表 + 时间表搬入 L2,然后清空本层
    /// (数据已搬走)。返回搬走的词数。
    pub fn flush_into(&mut self, l2: &mut super::overlay_dict::OverlayDict) -> usize {
        let n = self.freq.len();
        l2.absorb(super::overlay_dict::FlushBatch {
            freq: std::mem::take(&mut self.freq),
            recent: std::mem::take(&mut self.recent),
        });
        n
    }

    /// 旧 memory 表迁移(round16 单表形态 → round19 分表):一行拆两表。
    pub fn load_legacy(&mut self, rows: Vec<(String, String, i64, u32, u64)>, now_ms: i64) {
        for (w, p, t, c, freq) in rows {
            if now_ms - t > T3D {
                continue;
            }
            self.freq.entry(w.clone()).or_insert_with(|| FreqEntry {
                pinyin: p,
                count: c,
                frequency: freq,
            });
            self.recent.entry(w).or_insert(t);
        }
    }

    /// 旧 recency 表迁移种子(仅时间表;首次提交后进频次通道)。
    pub fn load_legacy_recent(&mut self, rows: Vec<(String, i64)>, now_ms: i64) {
        for (w, t) in rows {
            if now_ms - t <= T3D {
                self.recent.entry(w).or_insert(t);
            }
        }
    }

    /// 会话复位:两表全清。
    pub fn clear(&mut self) {
        self.freq.clear();
        self.recent.clear();
    }

    /// 超上限时淘汰最旧条目(按时间表排序;两表同步裁剪)。
    fn evict_overflow(&mut self) {
        if self.freq.len() <= MAX_ENTRIES {
            return;
        }
        let mut oldest: Vec<(String, i64)> =
            self.recent.iter().map(|(w, t)| (w.clone(), *t)).collect();
        oldest.sort_by_key(|(_, t)| *t);
        let drop: std::collections::HashSet<String> = oldest
            .into_iter()
            .take(self.freq.len() - MAX_ENTRIES)
            .map(|(w, _)| w)
            .collect();
        for w in &drop {
            self.freq.remove(w);
            self.recent.remove(w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> i64 {
        1_800_000_000_000
    }

    #[test]
    fn record_commit_tracks_pair_time_count() {
        let mut m = MemoryLayer::default();
        let t = now();
        m.record_commit("中的", "zhongde", t);
        m.record_commit("中的", "", t + 1_000);
        let e = m.freq_entry("中的").unwrap();
        assert_eq!(e.pinyin, "zhongde", "映射对被记录(空拼音不覆盖)");
        assert_eq!(m.recent_ms("中的"), Some(t + 1_000), "时间表独立盖章");
        assert_eq!(e.count, 2, "累计次数");
        assert_eq!(e.frequency, 0, "None → 保留既有基础频率");
    }

    #[test]
    fn tier_decays_continuously() {
        let mut m = MemoryLayer::default();
        let t = now();
        m.record_commit("你", "ni", t - 5_000);
        m.record_commit("好", "hao", t - 60_000);
        m.record_commit("的", "de", t - 2 * 3_600_000);
        m.record_commit("中", "zhong", t - 12 * 3_600_000);
        m.record_commit("国", "guo", t - 2 * 86_400_000);
        let b_ni = m.tier("你", t);
        let b_hao = m.tier("好", t);
        let b_de = m.tier("的", t);
        let b_zhong = m.tier("中", t);
        let b_guo = m.tier("国", t);
        // 刚提交(分钟级)封顶在 RECENCY_MAX。
        assert!((b_ni - RECENCY_MAX).abs() < 1e-9);
        assert!((b_hao - RECENCY_MAX).abs() < 1e-6, "分钟级仍封顶(含 count 增强被 cap)");
        // 2h ≈ 4.63 + count 增強 1/3 ≈ 4.96(旧阶梯:整档 3)。
        assert!((b_de - 4.96).abs() < 0.05, "2h: {b_de}");
        // 12h ≈ 3.15 + 1/3 ≈ 3.48(旧阶梯:整档 2)。
        assert!((b_zhong - 3.48).abs() < 0.05, "12h: {b_zhong}");
        // 2d ≈ 0.78 + 1/3 ≈ 1.12(旧阶梯:整档 1)。
        assert!((b_guo - 1.12).abs() < 0.05, "2d: {b_guo}");
        // 连续性:单调递减,无同档同分。
        assert!(b_ni >= b_hao && b_hao > b_de && b_de > b_zhong && b_zhong > b_guo);
    }

    #[test]
    fn tier_same_age_differs_by_minutes() {
        // 连续化核心性质:同档窗口内几分钟之差也能分辨(旧阶梯同分)。
        let mut m = MemoryLayer::default();
        let t = now();
        // 5h 窗口(旧阶梯同为档 3):先用的衰减更多。
        m.record_commit("先用", "xianyong", t - 5 * 3_600_000);
        m.record_commit("后用", "houyong", t - 3 * 3_600_000);
        assert!(m.tier("后用", t) > m.tier("先用", t), "同档窗口内连续可分");
    }

    #[test]
    fn frequent_commit_boosts_tier_continuously() {
        let mut m = MemoryLayer::default();
        let t = now();
        for _ in 0..3 {
            m.record_commit("高频", "gaopin", t - 86_400_000);
        }
        m.record_commit("低频", "dipin", t - 86_400_000);
        let lo = m.tier("低频", t);
        let hi = m.tier("高频", t);
        // 1d:b_time ≈ 1.98;低频(count=1)≈ +1/3,高频(count=3)= +1.0。
        assert!((lo - 2.31).abs() < 0.05, "低频: {lo}");
        assert!((hi - 2.98).abs() < 0.05, "高频: {hi}");
        assert!(hi > lo, "count 线性增强(旧版 ≥3 阶跃的连续化)");
    }

    #[test]
    fn expired_entries_evicted() {
        let mut m = MemoryLayer::default();
        let t = now();
        m.record_commit("旧词", "", t - 4 * 86_400_000);
        m.record_commit("新词", "", t - 1_000);
        assert_eq!(m.tier("旧词", t), 0.0, ">3d 无加成");
        assert!(m.freq_entry("旧词").is_some(), "频率表仍在(分表:淘汰只发生在时间表)");
        assert!((m.tier("新词", t) - RECENCY_MAX).abs() < 1e-9);
    }

    #[test]
    fn effective_frequency_enhances_with_count() {
        let t = now();
        let mut m = MemoryLayer::default();
        m.register_self_generated("自生词", "zishengci"); // 基础 100
        let e0 = m.freq_entry("自生词").unwrap().clone();
        assert_eq!(e0.frequency, 100);
        assert_eq!(e0.effective_frequency(), 100, "count=0 → 无增强");
        for _ in 0..5 {
            m.record_commit("自生词", "", t);
        }
        let e = m.freq_entry("自生词").unwrap();
        assert_eq!(e.frequency, 100, "存储存基础值");
        assert_eq!(e.effective_frequency(), 150, "count=5 → +5×10");
        for _ in 0..1000 {
            m.record_commit("自生词", "", t);
        }
        let e = m.freq_entry("自生词").unwrap();
        assert_eq!(e.effective_frequency(), 100 + FREQ_ENHANCE_CAP, "增强封顶");
    }

    #[test]
    fn self_generated_starts_at_zero_count() {
        let mut m = MemoryLayer::default();
        let t = now();
        m.register_self_generated("自生词", "zishengci");
        assert_eq!(m.freq_entry("自生词").unwrap().count, 0);
        assert_eq!(m.tier("自生词", t), 0.0, "不在时间表 → 无近期加成");
        m.record_commit("自生词", "", t);
        assert_eq!(m.freq_entry("自生词").unwrap().count, 1);
    }

    #[test]
    fn lookup_pinyin_prefix() {
        let mut m = MemoryLayer::default();
        m.register_self_generated("中", "zhong");
        m.register_self_generated("重量", "zhongliang");
        m.register_self_generated("的", "de");
        assert_eq!(m.lookup_pinyin("zhong").len(), 2, "拼音前缀命中");
        assert!(m.lookup_pinyin("zhongx").is_empty());
        m.register_self_generated("hello", "");
        assert_eq!(m.lookup_pinyin("hello").len(), 0, "英文词不被拼音查询命中");
    }

    #[test]
    fn flush_moves_both_tables_and_clears() {
        let t = now();
        let mut l1 = MemoryLayer::default();
        l1.record_commit_weighted("你好", "nihao", t, Some(42_000));
        l1.register_self_generated("自生词", "zishengci");

        let mut l2 = super::super::overlay_dict::OverlayDict::default();
        let n = l1.flush_into(&mut l2);
        assert_eq!(n, 2, "频率表全部搬走");
        assert!(l1.is_empty() && l1.recent_ms("你好").is_none(), "L1 清空(数据已搬走)");

        // L2:频率表(含继承频率)+ 时间表都在。
        let e = l2.freq_entry("你好").unwrap();
        assert_eq!((e.pinyin.as_str(), e.frequency), ("nihao", 42_000));
        assert_eq!(l2.recent_ms("你好"), Some(t));
        assert_eq!(l2.freq_entry("自生词").unwrap().frequency, SELF_GEN_FREQUENCY);
    }

    #[test]
    fn legacy_migration_splits_row_into_two_tables() {
        let t = now();
        let mut m = MemoryLayer::default();
        m.load_legacy(
            vec![("有效".into(), "youxiao".into(), t - 1_000, 2, 7_000)],
            t,
        );
        m.load_legacy_recent(vec![("旧表词".into(), t - 60_000)], t);
        assert_eq!(m.freq_entry("有效").unwrap().frequency, 7_000);
        assert_eq!(m.recent_ms("有效"), Some(t - 1_000));
        assert!((m.tier("旧表词", t) - RECENCY_MAX).abs() < 0.01, "旧 recency 表迁移种子生效(仅时间表)");
        assert!(m.freq_entry("旧表词").is_none(), "旧表种子不进频率表");
    }
}

