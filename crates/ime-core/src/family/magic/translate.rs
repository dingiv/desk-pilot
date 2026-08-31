//! TranslateMember — `#translate`:把上游候选翻译后**替换**预测列表。
//!
//! 链式上下文的内置消费者(`#asr'#translate`):上游链(`#asr` 语音候选、
//! 拼音预测、`#clip` 剪贴板…)的**高亮首选**作为待译文本;路径参数是目标
//! 语言(`#translate/en` → 英文、`/ja` → 日文)。
//!
//! MVP(无模型):任何文本进来都标记 `[已翻译]`(语言参数体现在标记里,
//! `#translate/en` → `[已翻译:en]`)—— 端到端链路(`#asr'#translate` 把
//! 语音高亮传给翻译)的联调桩。接入真实翻译模型时只改 [`translate`]。

use super::member::{ChainContext, CommandArgs, ContextKind, MagicMember, Prediction};
use super::FamilyEnv;

pub struct TranslateMember;

impl TranslateMember {
    pub fn new() -> Self {
        TranslateMember
    }

    /// 命令输入(`#translate/en`)→ 路径参数(目标语言)。
    fn lang_of(input: &str) -> Option<String> {
        let rest = input
            .strip_prefix('#')
            .and_then(|r| r.strip_prefix("translate"))
            .unwrap_or("");
        CommandArgs::parse(rest).path.into_iter().next()
    }
}

impl Default for TranslateMember {
    fn default() -> Self {
        Self::new()
    }
}

/// MVP 翻译桩:文本 → `[已翻译] 文本`(带语言则 `[已翻译:en] 文本`)。
/// 真实实现接模型后替换此处。
fn translate(text: &str, lang: Option<&str>) -> String {
    match lang.filter(|l| !l.is_empty()) {
        Some(l) => format!("[已翻译:{l}] {text}"),
        None => format!("[已翻译] {text}"),
    }
}

impl MagicMember for TranslateMember {
    fn name(&self) -> &'static str {
        "translate"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__TRANSLATE__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(TranslateMember)
    }

    /// 感知上游(First):拿高亮首选,输出替换。
    fn wants_context(&self) -> Option<ContextKind> {
        Some(ContextKind::First)
    }

    fn predict_with_context(
        &mut self,
        _ctx: usize,
        input: &str,
        upstream: &ChainContext,
        _env: &dyn FamilyEnv,
    ) -> Vec<Prediction> {
        let text = upstream.first_text();
        if text.is_empty() {
            return vec![Prediction::interactive("(上游无候选 — 用法:#asr'#translate)")];
        }
        let lang = Self::lang_of(input);
        vec![Prediction::commit(translate(text, lang.as_deref()))]
    }

    /// 无上游的单独调用:提示链式用法(选中不上屏)。
    fn predict(&mut self, _ctx: usize, _input: &str, _env: &dyn FamilyEnv) -> Vec<Prediction> {
        vec![Prediction::interactive("用法:上游'#translate[/目标语言]")]
    }

    fn tick(&mut self, ctx: usize, buffer: &str, env: &dyn FamilyEnv) -> Option<Vec<Prediction>> {
        None
    }
}
