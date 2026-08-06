//! scoring — 可配置的打分参数。
//!
//! 之前这些值散落写死在各个 family / 模块里(家族优先级、recency 五档 boost、
//! bigram boost 上限、freq→score 映射),无法在不改代码的情况下调整。现在聚合为
//! [`ScoringConfig`],由构造者(fcitx5 前端读 `swift-ime.yaml`)注入引擎。
//! `Default` 与旧的写死值完全一致——不配置时行为不变。

/// 家族优先级(最终分 = raw_score × priority/100)。
///
/// 拼音、英文、emoji 参与统一打分(中英混输 + emoji 竞争):`#`/`/` 强制前缀
/// 分流后,魔法命令与 snippet 的候选由 FSM 直接填充,不再经过 scorer ——
/// 它们不参与这里的排序。
#[derive(Debug, Clone, Copy)]
pub struct FamilyPriorities {
    pub pinyin: u32,
    pub english: u32,
    pub emoji: u32,
}

impl Default for FamilyPriorities {
    fn default() -> Self {
        FamilyPriorities { pinyin: 100, english: 70, emoji: 60 }
    }
}

impl FamilyPriorities {
    /// Priority override for a family by name; `None` → the family's own
    /// hardcoded `priority()` (e.g. future families without a config entry).
    pub fn get(&self, family: &str) -> Option<u32> {
        match family {
            "pinyin" => Some(self.pinyin),
            "english" => Some(self.english),
            "emoji" => Some(self.emoji),
            _ => None,
        }
    }
}

/// Recent member 的时间分档 boost —— 按"距上次使用的时间"衰减:
/// 10s 内最热(0.20),1h/5h/1d/3d 逐档递减;超过 3d 的条目被移出(无加成)。
#[derive(Debug, Clone, Copy)]
pub struct RecencyBoosts {
    /// 距上次使用 ≤ 10s
    pub within_10s: f64,
    /// ≤ 1h
    pub within_1h: f64,
    /// ≤ 5h
    pub within_5h: f64,
    /// ≤ 1d
    pub within_1d: f64,
    /// ≤ 3d
    pub within_3d: f64,
}

impl Default for RecencyBoosts {
    fn default() -> Self {
        RecencyBoosts {
            within_10s: 0.20,
            within_1h: 0.15,
            within_5h: 0.10,
            within_1d: 0.05,
            within_3d: 0.02,
        }
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
        assert_eq!(s.priorities.emoji, 60);
        assert_eq!(s.recency.within_10s, 0.20);
        assert_eq!(s.recency.within_3d, 0.02);
        assert_eq!(s.bigram.max_boost, 0.25);
        assert_eq!(s.freq_scale.max_weight, 0.0, "auto by default");
        assert_eq!((s.freq_scale.min_score, s.freq_scale.max_score), (0.25, 1.0));
    }
}
