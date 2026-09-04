//! scoring — 可配置的打分参数(归属 family:打分是家族排序的词汇)。
//!
//! 之前这些值散落写死在各个 family / 模块里(家族优先级、recency 五档 boost、
//! bigram boost 上限、freq→score 映射),无法在不改代码的情况下调整。现在聚合为
//! [`ScoringConfig`],由构造者(fcitx5 前端读 `swift-ime.yaml`)注入引擎
//! (`ImeEngine::with_config` → 各家族)。`Default` 与旧的写死值完全一致——
//! 不配置时行为不变。
//!
//! 依赖方向:family 拥有本文件(round9 R4 同则 —— family 需要什么由 family
//! 定义);engine/apps 只消费注入。

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
        FamilyPriorities {
            pinyin: 100,
            english: 70,
            emoji: 60,
        }
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

/// 前缀联想的共享距离衰减 —— 联想词比输入长越多越不可信。
///
/// `diff` = 联想词拼音/关键词长度 − 输入长度(字符数)。**前 3 个字符的
/// 剩余免费**(覆盖"半截声母到完整音节"的典型差,zh→zhong 差 3):更近的
/// 联想与目标词拼词频;超出部分按 0.85^超出 衰减,宽前缀捞到的高频长词
/// (jix→jixiaokao)自然沉底。"打全了联想让位"由 pinyin 侧的条件折扣负责
/// (round10 W4:同查询存在 Full 精确命中时 prefix 再折一次 prefix_lookup
/// —— 距离衰减分不开"免费区内的长尾联想"与"低频精确命中")。
///
/// pinyin(前缀联想)与 emoji(关键词前缀)共用此公式;english 的前缀是
/// **质量式**(0.60 地板 + 0.25×词频×匹配率,无距离项)—— 语义不同,
/// 表达"这个词本身多常用、匹配多完整",不是"联想多可信",故不共用。
pub fn prefix_decay(diff_chars: usize) -> f64 {
    0.85_f64.powf(diff_chars.saturating_sub(PREFIX_DECAY_FREE) as f64)
}

/// 前缀衰减的免费额度(字符)。
pub const PREFIX_DECAY_FREE: usize = 3;

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
        // max_score 0.90:顶流词封顶在 0.90,给 recent/context 合成公式留
        // 加成空间((1-a)(a+b)/8+a 在 a→1 时失效);1.0 会让顶流顶满、
        // 失去所有加成(违背 yaml 声明的 "top candidates 0.70–0.85" 原则)。
        FreqScale {
            max_weight: 0.0,
            min_score: 0.25,
            max_score: 0.90,
        }
    }
}

/// 全部可配置打分参数的聚合,注入 `ImeEngine::with_config`。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoringConfig {
    pub priorities: FamilyPriorities,
    pub freq_scale: FreqScale,
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
        assert_eq!(s.freq_scale.max_weight, 0.0, "auto by default");
        assert_eq!(
            (s.freq_scale.min_score, s.freq_scale.max_score),
            (0.25, 0.90)
        );
    }
}
