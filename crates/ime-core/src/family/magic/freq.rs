//! FreqMember — `#freq/up` / `#freq/down`:手工微调词条频率(round22 ④)。
//!
//! 链式用法:`yibu'#freq/up` —— 用户在拼音面板把高亮移到目标词
//! (如「异步」)后键入 `'` 开链,分链那一刻的高亮词被锚定为操作对象
//! (见 `fsm::family_prediction` 的 chain_anchor);`#freq` 消费上游
//! 首选 + 链根文本(拼音),对 (yibu ↔ 异步) 映射对在 overlay 增量
//! 账本上记一笔 ±[`FREQ_MANUAL_STEP`](crate::store::memory::FREQ_MANUAL_STEP)。
//!
//! 幂等性:同一 (命令, 词) 只记一次账;改词或改方向才重新记账
//! (predict 会在链式刷新中被反复调用)。
//!
//! 无上游单独调用(`#freq/up` 直接打):提示链式用法。

use super::member::{ChainContext, ContextKind, MagicMember, Prediction};
use super::FamilyEnv;
use crate::store::memory::FREQ_MANUAL_STEP;

pub struct FreqMember {
    /// 已记账的 (命令全文, 词):防链式刷新重复记账。
    applied: Option<(String, String)>,
}

impl FreqMember {
    pub fn new() -> Self {
        FreqMember { applied: None }
    }

    /// 命令输入(`#freq/up`)→ 方向(+1 / −1 / None)。
    fn direction_of(input: &str) -> Option<i64> {
        match input.trim_start_matches('#') {
            "freq/up" => Some(1),
            "freq/down" => Some(-1),
            _ => None,
        }
    }
}

impl Default for FreqMember {
    fn default() -> Self {
        Self::new()
    }
}

impl MagicMember for FreqMember {
    fn name(&self) -> &'static str {
        "freq"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__FREQ__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(FreqMember::new())
    }

    /// 完整触发路径:up / down 两个方向各一条。
    fn registered_paths(&self) -> Vec<String> {
        vec!["freq/up".into(), "freq/down".into()]
    }

    /// 感知上游(First):高亮锚定的词条 + 链根拼音。
    fn wants_context(&self) -> Option<ContextKind> {
        Some(ContextKind::First)
    }

    fn predict_with_context(
        &mut self,
        _ctx: usize,
        input: &str,
        upstream: &ChainContext,
        env: &dyn FamilyEnv,
    ) -> Vec<Prediction> {
        let Some(dir) = Self::direction_of(input) else {
            return vec![Prediction::interactive("(用法:#freq/up | #freq/down)")];
        };
        let word = upstream.first_text();
        if word.is_empty() {
            return vec![Prediction::interactive("(上游无候选 — 用法:yibu'#freq/up)")];
        }
        // 幂等:同 (命令, 词) 不重复记账。
        if self.applied.as_ref().map(|(c, w)| (c.as_str(), w.as_str())) == Some((input, word)) {
            return vec![Prediction::interactive(&format!(
                "(已调整 {word} — 见上一预览)"
            ))];
        }
        self.applied = Some((input.to_string(), word.to_string()));
        let Some((before, after)) =
            env.adjust_word_freq(&upstream.root_text, word, dir * FREQ_MANUAL_STEP)
        else {
            return vec![Prediction::interactive("(词频调整未接线)")];
        };
        let arrow = if dir > 0 { "↑" } else { "↓" };
        // 提交即上屏该词(自然闭环:用户调的正是想用的词,
        // 提交还会按正常路径记一笔有机增量)。
        vec![Prediction::commit_raw(
            format!("{word} {arrow} 词频 {before} → {after}"),
            word,
        )]
    }

    /// 无上游的单独调用:提示链式用法(选中不上屏)。
    fn predict(&mut self, _ctx: usize, _input: &str, _env: &dyn FamilyEnv) -> Vec<Prediction> {
        vec![Prediction::interactive("用法:高亮目标词后 上游'#freq/up|down")]
    }

    fn tick(&mut self, _ctx: usize, _buffer: &str, _env: &dyn FamilyEnv) -> Option<Vec<Prediction>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_parses_paths() {
        assert_eq!(FreqMember::direction_of("#freq/up"), Some(1));
        assert_eq!(FreqMember::direction_of("#freq/down"), Some(-1));
        assert_eq!(FreqMember::direction_of("#freq"), None);
        assert_eq!(FreqMember::direction_of("#freq/sideways"), None);
    }
}
