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

/// 自生词默认基础频率(round22 ②:近似中频种子词 —— rime-ice 中频档
/// ≈ 12k~50k;旧值 100 使新造词起步垫底,与"用户造的词立刻可用"相悳)。
pub const SELF_GEN_FREQUENCY: u64 = 30_000;
/// 有机增量分子(round22 ③):第 n 次提交记 δ = GAIN_UNIT×rel/(n+SAT),
/// 累计 ≈ GAIN_UNIT×ln((n+SAT)/SAT) —— 对数增长从**记账方式**中自然
/// 涌现,读取时不算公式;误选 1~2 次影响微小(容错)。
pub const FREQ_GAIN_UNIT: f64 = 8_000.0;
/// 调和级数饱和偏移(首笔不至于过大:n=1 → δ≈GAIN_UNIT/5×rel)。
pub const FREQ_HARMONIC_SAT: f64 = 4.0;
/// 有机增量总量封顶(round22:50k ≈ rime 顶流的 1/10,习惯词升到
/// 高频档但压不过「的/了」级超高频,不霸榜)。
pub const DELTA_ORGANIC_CAP: i64 = 50_000;
/// 手工调整步长(#freq/up|down,round22 ④):一笔 ≈ 30 次使用的累计量。
pub const FREQ_MANUAL_STEP: i64 = 25_000;
/// 增量总上限(有机 + 手工合计;手工允许突破有机封顶)。round23 放宽到
/// ±100 万:freq_to_score 是 log₂ 刻度,种子词 base 数万~数十万,旧上限
/// 10 万在刻度上只值 ~0.02 分 —— `#freq/down/100000` 必须真的压得动。
pub const DELTA_CEIL: i64 = 1_000_000;
/// 负向下限(round23 修正:绝对地板 1,允许强力降权)。旧 round22 的
/// base×0.5 相对下限对多字词(手工等级 base 仅几千)只挪 ~0.02 分,
/// `#freq/down/50000` 压不动 —— 用户语义是"把它摁下去",账本本身仍夹
/// 在 ±DELTA_CEIL 内,可反复 up 恢复。
/// 相对增量夹限(round22 ⑤):rel = count/均值 ∈ [0.5, 2.0]。
pub const REL_FLOOR: f64 = 0.5;
pub const REL_CEIL: f64 = 2.0;
/// flush 阈值(round19):L1 频率表攒满即整体搬入 L2 并清空。
pub const FLUSH_THRESHOLD: usize = 128;

/// 频率统计表的一行(round22 增量账本):映射对 + 不可改写的基础频率
/// + 可正可负的增量 + 原始计数。
///
/// (时间统计独立在 [`MemoryLayer::recent`] 表 —— round19 分表指令。)
#[derive(Debug, Clone, PartialEq)]
pub struct FreqEntry {
    /// 提交时的拼音(映射对的另一半;英文词为空)。
    pub pinyin: String,
    /// **基础频率**(round22 ①:永不改写):种子词 = 首次继承的 SeedDict
    /// 原始频率;自生词 = [`SELF_GEN_FREQUENCY`];0 = 尚无(旧迁移
    /// 种子),覆盖算法不作用于 0。与 FST value 同量纲,经
    /// `freq_to_score` 线性重标后才参与排序。
    pub base: u64,
    /// **增量账本**(round22 ③④):提交记正笔(调和级数×rel),
    /// #freq 手工记账可正可负;全体单词共享同一条账本语义。
    pub delta: i64,
    /// 累计提交次数(原始统计:诊断 + ⑤ 相对增量的分母材料,
    /// 不再直接驱动分数)。
    pub count: u32,
}

/// 有效频率 = 基础 + 增量,夹在 [1, 基础+[`DELTA_CEIL`]]
///(负向可压到地板分 —— 降权要真的降得动;正向有总顶,不霸榜)。
pub fn effective_frequency(base: u64, delta: i64) -> u64 {
    if base == 0 {
        return 0;
    }
    let ceil = base as i64 + DELTA_CEIL;
    (base as i64 + delta).clamp(1, ceil).max(0) as u64
}

impl FreqEntry {
    /// **有效频率** = 基础 + 增量(见 [`effective_frequency`])。
    pub fn effective_frequency(&self) -> u64 {
        effective_frequency(self.base, self.delta)
    }
}

/// 衰减窗口(wall-clock 毫秒):超 3 天的近期加成归零(惰性移出 ——
/// **只删时间表**,频率表的统计不受影响)。
const T3D: i64 = 259_200_000;

/// 连续衰减半衰期(round20):g(t) = 2^(−t/HALF_LIFE),每 18h 减半
/// (对齐旧五档阶梯的中段手感:5h≈0.82、1d≈0.40、3d≈0.06)。
pub const RECENCY_HALF_LIFE_MS: i64 = 64_800_000;

/// 近期增益上限(round21 单公式):score' = a + (1-a) × g × GAIN_MAX。
/// 取 0.70 对齐旧 z 公式 b=5 档的等效增益((a+5)/8 ≈ 0.67~0.74)。
pub const RECENCY_GAIN_MAX: f64 = 0.70;

/// 记录上限(3 天窗口内使用词的自然上限;保险起见截断)。
const MAX_ENTRIES: usize = 512;

/// L1 工作集:频率统计表 + 时间统计表(独立,不是一张表)。
#[derive(Default)]
pub struct MemoryLayer {
    /// 频率统计:词 → (拼音, 基础频率, 增量, 累计次数)。
    freq: HashMap<String, FreqEntry>,
    /// 时间统计:词 → 最近提交 wall-clock ms。
    recent: HashMap<String, i64>,
    /// Σcount 累加器(⑤ 相对增量的分母材料;增删改时同步维护)。
    total_count: u64,
    /// 频率表生成号(round24):任何 freq 表变更(记录/登记/手工调整/淘汰/
    /// flush)递增 —— lattice 旁路词典据此感知 L1 变化(自生词刚造出来
    /// 就能混写/简拼召回,不等 flush 进 L2)。
    gen: u64,
}

impl MemoryLayer {
    /// 词条平均提交次数(⑤ rel 分母;空表返回 1 防除零)。
    fn mean_count(&self) -> f64 {
        if self.freq.is_empty() {
            return 1.0;
        }
        (self.total_count as f64 / self.freq.len() as f64).max(1.0)
    }

    /// 后处理提交登记:频率表计数 +1(并设定映射对/继承频率),
    /// 时间表独立盖章。
    pub fn record_commit(&mut self, word: &str, pinyin: &str, now_ms: i64) {
        self.record_commit_weighted(word, pinyin, now_ms, None);
    }

    /// 带频率版提交登记(round22 增量账本):
    /// - `base = Some(f)` 仅在首次登记(base==0)时生效 —— **基础频率
    ///   永不改写**(①);既有条目的后续提交只动 count 与 delta。
    /// - 每次提交记一笔正增量:δ = GAIN_UNIT × rel / (count+SAT),
    ///   rel = count/全体均值 ∈ [0.5,2.0](⑤ 相对增量:同样 30 次,
    ///   冷门词库里的重度偏好满加成,热门词库里温和加成;别人用得
    ///   多 → 自己的 rel 走低 —— 有机负向,无需事件级惩罚)。
    ///   调和级数累计 → 对数增长;有机总量封顶 [`DELTA_ORGANIC_CAP`]。
    pub fn record_commit_weighted(
        &mut self,
        word: &str,
        pinyin: &str,
        now_ms: i64,
        base: Option<u64>,
    ) {
        if word.is_empty() {
            return;
        }
        let mean = self.mean_count();
        // 频率表:计数 + 首次继承基础频率 + 映射对。
        let e = self
            .freq
            .entry(word.to_string())
            .or_insert_with(|| FreqEntry {
                pinyin: String::new(),
                base: 0,
                delta: 0,
                count: 0,
            });
        if !pinyin.is_empty() {
            e.pinyin = pinyin.to_string();
        }
        if let Some(f) = base {
            if e.base == 0 {
                e.base = f.max(1);
            }
        }
        e.count = e.count.saturating_add(1);
        self.total_count = self.total_count.saturating_add(1);
        // 正向记账(③⑤):rel 用记账后的 count/记账前的均值。
        let rel = (e.count as f64 / mean).clamp(REL_FLOOR, REL_CEIL);
        let incr = (FREQ_GAIN_UNIT * rel / (e.count as f64 + FREQ_HARMONIC_SAT)) as i64;
        e.delta = (e.delta + incr).min(DELTA_ORGANIC_CAP);
        self.bump();
        // 时间表:独立一行(分表,不与频率统计混存)。
        self.recent.insert(word.to_string(), now_ms);
        self.evict_overflow();
    }

    /// 手工调整记账(round22 ④,#freq/up|down 魔法命令):
    /// 一笔 ±[`FREQ_MANUAL_STEP`];`seed_base` 在词条尚无基础频率时
    /// 补继承(种子词查 lattice,无则自生词默认)。计入总顶
    /// [`DELTA_CEIL`] / 底限 base×0.5(在 effective 读取时夹取)。
    /// 不动 count / mean(手工调整不算使用行为)。
    pub fn apply_manual_adjust(&mut self, word: &str, pinyin: &str, step: i64, seed_base: Option<u64>) {
        if word.is_empty() {
            return;
        }
        let e = self
            .freq
            .entry(word.to_string())
            .or_insert_with(|| FreqEntry {
                pinyin: String::new(),
                base: seed_base.unwrap_or(SELF_GEN_FREQUENCY).max(1),
                delta: 0,
                count: 0,
            });
        if !pinyin.is_empty() {
            e.pinyin = pinyin.to_string();
        }
        if e.base == 0 {
            e.base = seed_base.unwrap_or(SELF_GEN_FREQUENCY).max(1);
        }
        e.delta = (e.delta + step).clamp(-DELTA_CEIL, DELTA_CEIL);
        self.bump();
    }

    /// 自生词登记(后处理学习路径):入册但不计提交(count = 0),
    /// 基础频率 = 中频档默认(round22 ②)。已有条目只补拼音,
    /// 不动计数与既有账本。
    pub fn register_self_generated(&mut self, word: &str, pinyin: &str) {
        if word.is_empty() {
            return;
        }
        let fresh = !self.freq.contains_key(word);
        let e = self
            .freq
            .entry(word.to_string())
            .or_insert_with(|| FreqEntry {
                pinyin: String::new(),
                base: SELF_GEN_FREQUENCY,
                delta: 0,
                count: 0,
            });
        if !pinyin.is_empty() {
            e.pinyin = pinyin.to_string();
        }
        if fresh {
            self.bump();
        }
    }

    /// 频率表生成号(旁路词典同步指纹的一半;另一半在 L2)。
    pub fn generation(&self) -> u64 {
        self.gen
    }

    /// 频率表生成号递增(freq 表结构/内容变更)。
    fn bump(&mut self) {
        self.gen = self.gen.wrapping_add(1);
    }

    /// 频率表整表迭代(旁路词典合并同步用:L1 ∪ L2,L1 更热覆盖同词)。
    pub fn freq_iter(
        &self,
    ) -> impl Iterator<Item = (&String, &FreqEntry)> {
        self.freq.iter()
    }

    /// 近期增益(round21 单公式):`g = GAIN_MAX × 2^(−age/半衰期)`,
/// 直接就是合成公式的系数(`score' = a + (1-a) × g`),取代旧三段链
    /// (档位 b → z 合成)与 count 双计(频次增强只在
    /// [`FreqEntry::effective_frequency`] 一处)。超 3 天惰性移出
    /// (消减)——**只删本层时间表条目,频率表统计不动**。
    pub fn recency_boost(&mut self, word: &str, now_ms: i64) -> f64 {
        let Some(&last) = self.recent.get(word) else {
            return 0.0;
        };
        let age = now_ms - last;
        if age > T3D {
            // 超过 3d:移出时间表(惰性淘汰),不再有加成;
            // 频率表(freq)不受影响 —— 分表语义的核心不变式。
            self.recent.remove(word);
            return 0.0;
        }
        RECENCY_GAIN_MAX * f64::exp2(-(age as f64) / RECENCY_HALF_LIFE_MS as f64)
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
        self.total_count = 0;
        self.bump();
        n
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
        self.bump();
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
        assert_eq!(e.base, 0, "None → 不设基础频率");
    }

    #[test]
    fn recency_boost_decays_continuously() {
        let mut m = MemoryLayer::default();
        let t = now();
        m.record_commit("你", "ni", t - 5_000);
        m.record_commit("好", "hao", t - 60_000);
        m.record_commit("的", "de", t - 2 * 3_600_000);
        m.record_commit("中", "zhong", t - 12 * 3_600_000);
        m.record_commit("国", "guo", t - 2 * 86_400_000);
        let g_ni = m.recency_boost("你", t);
        let g_hao = m.recency_boost("好", t);
        let g_de = m.recency_boost("的", t);
        let g_zhong = m.recency_boost("中", t);
        let g_guo = m.recency_boost("国", t);
        // 刚提交(分钟级)≈ GAIN_MAX。
        assert!((g_ni - RECENCY_GAIN_MAX).abs() < 1e-4, "5s 近顶: {g_ni}");
        assert!((g_hao - RECENCY_GAIN_MAX * 0.9999).abs() < 1e-3, "分钟级仍近顶");
        // g(t) = GAIN_MAX × 2^(−t/18h):2h≈0.553、12h≈0.278、2d≈0.11。
        assert!((g_de - 0.70 * 0.9258).abs() < 1e-3, "2h: {g_de}");
        assert!((g_zhong - 0.70 * 0.6300).abs() < 1e-3, "12h: {g_zhong}");
        assert!((g_guo - 0.70 * 0.1575).abs() < 1e-3, "2d: {g_guo}");
        // 连续性:单调递减,无同档同分。
        assert!(g_ni >= g_hao && g_hao > g_de && g_de > g_zhong && g_zhong > g_guo);
        // 半衰期锚点:18h 处恰减半。
        let mut m2 = MemoryLayer::default();
        m2.record_commit("半", "ban", t - RECENCY_HALF_LIFE_MS);
        assert!((m2.recency_boost("半", t) - RECENCY_GAIN_MAX / 2.0).abs() < 1e-9);
    }

    #[test]
    fn recency_boost_same_age_differs_by_minutes() {
        // 连续化核心性质:同档窗口内几分钟之差也能分辨(旧阶梯同分)。
        let mut m = MemoryLayer::default();
        let t = now();
        m.record_commit("先用", "xianyong", t - 5 * 3_600_000);
        m.record_commit("后用", "houyong", t - 3 * 3_600_000);
        assert!(m.recency_boost("后用", t) > m.recency_boost("先用", t), "同档窗口内连续可分");
    }

    #[test]
    fn recency_boost_is_pure_time_count_lives_in_freq_domain() {
        // round21 去双计:count 不再进时间增益,只走 effective_frequency。
        let mut m = MemoryLayer::default();
        let t = now();
        for _ in 0..10 {
            m.record_commit_weighted("高频", "gaopin", t - 86_400_000, Some(50_000));
        }
        m.record_commit_weighted("低频", "dipin", t - 86_400_000, Some(50_000));
        // 同 age、不同 count → 增益相同(差异由 effective_frequency 承担)。
        let g_hi = m.recency_boost("高频", t);
        let g_lo = m.recency_boost("低频", t);
        assert!((g_hi - g_lo).abs() < 1e-12, "时间增益不含 count: {g_hi} vs {g_lo}");
        let e_hi = m.freq_entry("高频").unwrap().effective_frequency();
        let e_lo = m.freq_entry("低频").unwrap().effective_frequency();
        assert!(e_hi > e_lo, "频次差异由频率域承担: {e_hi} vs {e_lo}");
    }

    #[test]
    fn expired_entries_evicted() {
        let mut m = MemoryLayer::default();
        let t = now();
        m.record_commit("旧词", "", t - 4 * 86_400_000);
        m.record_commit("新词", "", t - 1_000);
        assert_eq!(m.recency_boost("旧词", t), 0.0, ">3d 无加成");
        assert!(m.freq_entry("旧词").is_some(), "频率表仍在(分表:淘汰只发生在时间表)");
        assert!((m.recency_boost("新词", t) - RECENCY_GAIN_MAX).abs() < 1e-4);
    }

    #[test]
    #[test]
    fn delta_ledger_grows_harmonically_and_caps() {
        let t = now();
        let mut m = MemoryLayer::default();
        m.register_self_generated("自生词", "zishengci"); // 中频基础 30_000
        let e0 = m.freq_entry("自生词").unwrap().clone();
        assert_eq!(e0.base, SELF_GEN_FREQUENCY);
        assert_eq!(e0.effective_frequency(), SELF_GEN_FREQUENCY, "count=0 → 无增量");
        // 首笔:mean=1 → rel=1,δ = 8000×1/(1+4) = 1600(精确锚点)。
        m.record_commit("自生词", "", t);
        assert_eq!(m.freq_entry("自生词").unwrap().delta, 1_600, "首笔精确值");
        for _ in 0..4 {
            m.record_commit("自生词", "", t);
        }
        let e = m.freq_entry("自生词").unwrap();
        assert_eq!(e.base, SELF_GEN_FREQUENCY, "基础频率不改写(①)");
        // 次线性:5 笔累计 < 5×首笔(调和级数,非线性叠加)。
        assert!(e.delta > 6_000 && e.delta < 9_000, "调和级数累计(③,次线性<5×1600): {}", e.delta);
        for _ in 0..5_000 {
            m.record_commit("自生词", "", t);
        }
        let e = m.freq_entry("自生词").unwrap();
        assert_eq!(e.delta, DELTA_ORGANIC_CAP, "有机增量封顶");
        assert!(e.effective_frequency() <= SELF_GEN_FREQUENCY + DELTA_ORGANIC_CAP as u64);
    }

    #[test]
    fn manual_adjust_moves_delta_both_ways() {
        let t = now();
        let mut m = MemoryLayer::default();
        // #freq/up:种子词补继承 base。
        m.apply_manual_adjust("异步", "yibu", FREQ_MANUAL_STEP, Some(100_000));
        let e = m.freq_entry("异步").unwrap();
        assert_eq!((e.base, e.delta, e.count), (100_000, FREQ_MANUAL_STEP, 0), "手工不计 count");
        m.apply_manual_adjust("异步", "yibu", FREQ_MANUAL_STEP, Some(100_000));
        assert_eq!(m.freq_entry("异步").unwrap().delta, 2 * FREQ_MANUAL_STEP);
        // 连续 down 穿过 0:底限在 effective 读取时夹取,账本只夹 ±CEIL
        //(round23 CEIL 放宽到 ±100 万;2×25k − 12×25k = −250k 未触底)。
        for _ in 0..12 {
            m.apply_manual_adjust("异步", "yibu", -FREQ_MANUAL_STEP, None);
        }
        let e = m.freq_entry("异步").unwrap();
        assert_eq!(e.delta, -10 * FREQ_MANUAL_STEP, "2 上 12 下 = −250k(未触 −CEIL)");
        assert_eq!(
            e.effective_frequency(),
            1,
            "有效频率可压到绝对地板(降权要降得动;账本可 up 恢复)"
        );
    }

    #[test]
    fn rel_normalizes_against_corpus_mean() {
        // ⑤:同样 count=3,冷门词库(均值低)满加成,热门词库(均值高)减半。
        let t = now();
        let mut cold = MemoryLayer::default();
        for _ in 0..3 {
            cold.record_commit_weighted("唯一", "weiyi", t, Some(50_000));
        }
        let mut hot = MemoryLayer::default();
        for w in ["a", "b", "c", "d", "e", "f", "g"] {
            for _ in 0..30 {
                hot.record_commit_weighted(w, "", t, Some(50_000));
            }
        }
        for _ in 0..3 {
            hot.record_commit_weighted("新词", "xinci", t, Some(50_000));
        }
        let d_cold = cold.freq_entry("唯一").unwrap().delta;
        let d_hot = hot.freq_entry("新词").unwrap().delta;
        assert!(d_cold > d_hot, "冷库 rel 高、热库 rel 低: {d_cold} vs {d_hot}");
    }

    #[test]
    fn self_generated_starts_at_zero_count() {
        let mut m = MemoryLayer::default();
        let t = now();
        m.register_self_generated("自生词", "zishengci");
        assert_eq!(m.freq_entry("自生词").unwrap().count, 0);
        assert_eq!(m.recency_boost("自生词", t), 0.0, "不在时间表 → 无近期加成");
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
        assert_eq!((e.pinyin.as_str(), e.base), ("nihao", 42_000));
        assert_eq!(l2.recent_ms("你好"), Some(t));
        assert_eq!(l2.freq_entry("自生词").unwrap().base, SELF_GEN_FREQUENCY);
    }

}

