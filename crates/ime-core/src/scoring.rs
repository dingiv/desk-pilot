//! scoring — 可配置的打分参数。
//!
//! 之前这些值散落写死在各个 family / 模块里(家族优先级、recency 五档 boost、
//! bigram boost 上限、freq→score 映射),无法在不改代码的情况下调整。现在聚合为
//! [`ScoringConfig`],由构造者(fcitx5 前端读 `swift-ime.yaml`)注入引擎。
//! `Default` 与旧的写死值完全一致——不配置时行为不变。

/// 家族优先级(最终分 = raw_score × priority/100)。
///
/// 只有拼音与英文参与统一打分(中英混输竞争):`#`/`/` 强制前缀分流后,
/// 魔法命令与 snippet 的候选由 FSM 直接填充,不再经过 scorer —— 它们不
/// 参与这里的排序。
#[derive(Debug, Clone, Copy)]
pub struct FamilyPriorities {
    pub pinyin: u32,
    pub english: u32,
}

impl Default for FamilyPriorities {
    fn default() -> Self {
        FamilyPriorities { pinyin: 100, english: 70 }
    }
}

impl FamilyPriorities {
    /// Priority override for a family by name; `None` → the family's own
    /// hardcoded `priority()` (e.g. future families without a config entry).
    pub fn get(&self, family: &str) -> Option<u32> {
        match family {
            "pinyin" => Some(self.pinyin),
            "english" => Some(self.english),
            _ => None,
        }
    }
}

/// RecencyStore 的位置衰减 boost(按候选在最近提交环中的位置)。
#[derive(Debug, Clone, Copy)]
pub struct RecencyBoosts {
    /// 最近一次提交 (pos=0)
    pub pos0: f64,
    /// 第二次 (pos=1)
    pub pos1: f64,
    /// 第三次 (pos=2)
    pub pos2: f64,
    /// pos 3..=9
    pub mid: f64,
    /// pos 10..=63
    pub far: f64,
}

impl Default for RecencyBoosts {
    fn default() -> Self {
        RecencyBoosts { pos0: 0.20, pos1: 0.15, pos2: 0.10, mid: 0.05, far: 0.02 }
    }
}

/// UserBigram 上下文 boost 的归一化上限。
#[derive(Debug, Clone, Copy)]
pub struct BigramTuning {
    /// 最高频 bigram 的加成倍数上限(1.0 = 无加成,1.25 = +25%)。
    pub max_boost: f64,
}

impl Default for BigramTuning {
    fn default() -> Self {
        BigramTuning { max_boost: 0.25 }
    }
}

/// 字典词频 → 内部分值的映射参数。
///
/// 映射为 log₂ 归一化:`score = log2(freq+1) / log2(max+1)`,再 clamp 到
/// [min_score, max_score]。`max_weight` 控制分母:
/// - `0`(默认)= **auto**:使用构建索引时记录的实际最大词频(随 cache v2 持久化),
///   映射始终对齐真实数据分布——词频 501276 与 500369 不再被压成同分;
/// - `> 0` = 显式覆盖(例如固定 `600000`)。
///
/// `min_score`/`max_score` 收紧 clamp 范围会牺牲顶部/底部区分度,一般保持默认。
#[derive(Debug, Clone, Copy)]
pub struct FreqScale {
    /// 0 = auto(索引构建时的实际最大词频);>0 = 显式固定分母。
    pub max_weight: f64,
    pub min_score: f64,
    pub max_score: f64,
}

impl Default for FreqScale {
    fn default() -> Self {
        FreqScale { max_weight: 0.0, min_score: 0.25, max_score: 1.0 }
    }
}

/// 全部可配置打分参数的聚合,注入 `ImeEngine::with_config`。
#[derive(Debug, Clone, Copy)]
pub struct ScoringConfig {
    pub priorities: FamilyPriorities,
    pub recency: RecencyBoosts,
    pub bigram: BigramTuning,
    pub freq_scale: FreqScale,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        ScoringConfig {
            priorities: FamilyPriorities::default(),
            recency: RecencyBoosts::default(),
            bigram: BigramTuning::default(),
            freq_scale: FreqScale::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_legacy_hardcoded_values() {
        // 不配置时行为必须与旧的写死值完全一致。
        let s = ScoringConfig::default();
        assert_eq!(s.priorities.pinyin, 100);
        assert_eq!(s.priorities.english, 70);
        assert_eq!(s.recency.pos0, 0.20);
        assert_eq!(s.recency.far, 0.02);
        assert_eq!(s.bigram.max_boost, 0.25);
        assert_eq!(s.freq_scale.max_weight, 0.0, "auto by default");
        assert_eq!((s.freq_scale.min_score, s.freq_scale.max_score), (0.25, 1.0));
    }
}
