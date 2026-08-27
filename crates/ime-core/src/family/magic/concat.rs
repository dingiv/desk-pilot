//! ConcatMember — `#concat`:把上游整页候选拼接成一个候选。
//!
//! 链式空链语法的内置消费者(`shijian''#concat`):空链(`''`)让框架把上游
//! **整页**候选作为上下文传入(而非高亮首选),本成员按顺序拼接全部条目,
//! 输出**替换**候选列表 —— 用户例:`shijian` 整页 [时间, 事件, 实践, 世间…]
//! → `时间事件实践世间…`。
//!
//! 单独使用(`#concat` 无上游)给出提示(空链语法是它存在的意义)。

use super::member::{ChainContext, ContextKind, MagicMember, Prediction};
use crate::state::{StateMachine, StepEnv};

pub struct ConcatMember;

impl ConcatMember {
    pub fn new() -> Self {
        ConcatMember
    }
}

impl Default for ConcatMember {
    fn default() -> Self {
        Self::new()
    }
}

impl MagicMember for ConcatMember {
    fn name(&self) -> &'static str {
        "concat"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__CONCAT__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(ConcatMember)
    }

    /// 消费上游整页(Page):全部候选按序拼接,直接替换为单条可提交候选。
    fn wants_context(&self) -> Option<ContextKind> {
        Some(ContextKind::Page)
    }

    fn predict_with_context(
        &mut self,
        _ctx: usize,
        _input: &str,
        upstream: &ChainContext,
        _env: &dyn StepEnv,
    ) -> Vec<Prediction> {
        if upstream.items.is_empty() {
            return vec![Prediction::interactive("(上游无候选)")];
        }
        let joined: String = upstream.items.concat();
        if joined.is_empty() {
            return vec![Prediction::interactive("(上游候选为空文本)")];
        }
        vec![Prediction::commit(joined)]
    }

    /// 无上游的单独调用:提示空链语法(选中不上屏)。
    fn predict(&mut self, _ctx: usize, _input: &str, _env: &dyn StepEnv) -> Vec<Prediction> {
        vec![Prediction::interactive("用法:上游''#concat(两个 ' 传整页)")]
    }

    fn tick(&mut self, _sm: &mut StateMachine, _env: &dyn StepEnv) -> Option<Vec<Prediction>> {
        None
    }
}
