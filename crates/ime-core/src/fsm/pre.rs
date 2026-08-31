//! Stage 1 系统控制(pre,round9 R3 自 state.rs 迁出):
//! 每枚键的第一站 —— 判定"输入法消费还是透传给应用"。
//!
//! [`ControlStage`] 是显式的 stage1 结构体(无内部状态,标志位寄存在
//! `StateMachine.flags`);它只做**系统级流转**(提交/选词/翻页/高亮/
//! 修饰键/命令文本 hoist),不产生预测 —— 字符预测一律交给
//! `FamilyPipeline::step`(stage2 家族分发)。

use super::family::FamilyPipeline;
use super::state::{KeyEvent, KeyKind, StateFlags, StateMachine};
use crate::frontend::{action, ImeView};
use crate::fsm::family::{ComposeState, StepEnv};

/// Stage 1 系统控制(显式结构体:stage1 是管线的一员,不是散落的 match)。
/// 无内部状态(零大小,Copy —— 表按值取用),行为即结构。
#[derive(Debug, Default, Clone, Copy)]
pub struct ControlStage;

impl ControlStage {
    /// 路由一枚键:系统键就地处理,字符键交给 stage2。返回的视图带明确
    /// action 位(NONE → HANDLED,防止退格漏到应用)。
    pub fn route_key(
        &self,
        table: &mut StateMachine,
        pipeline: &mut FamilyPipeline,
        key: KeyEvent,
        env: &dyn StepEnv,
    ) -> ImeView {
        let mut view = self.route_inner(pipeline, key, env);
        // 不变式:键路径返回的视图必须带明确的 action 位。组合状态机里
        // "消费了键但无可渲染"的路径(退格清空 buffer 后 reset、snippet
        // 退空、magic 成员退出)返回的是空视图 —— action 为 NONE 时前端
        // 会把键放行给应用(退格漏过去,应用里已输入的字被删掉)。
        if view.action == action::NONE {
            view.action = action::HANDLED;
        }
        table.flags = pipeline.state_flags();
        view
    }

    fn route_inner(
        &self,
        pipeline: &mut FamilyPipeline,
        key: KeyEvent,
        env: &dyn StepEnv,
    ) -> ImeView {
        // 1. Ctrl/Alt 组合键是应用快捷键(Ctrl+/ 注释、Ctrl+C 复制…),一律放行。
        //    修饰键策略在引擎内 —— 前端不再自行拦截。Shift 不在此列:大写
        //    字母/符号照常走字符路径(组合中是终止符,idle 透传)。
        if key.ctrl || key.alt {
            return FamilyPipeline::passthrough_view();
        }

        // 2. Snippet 态(组合 `#…` 命令):数字与 `+ - = [ ]` 是命令文本
        //    (`?num=2` 的 `=`、`#req` URL 的 `-`/`[`/`]`…)—— 由状态机决定
        //    (数字在可选中态选中候选,否则追加)。方向/翻页键仍导航。
        if pipeline.state == ComposeState::Snippet {
            // 命令文本字符(`?num=2` 的数字、`#req` URL 的 `-`/`[`/`]`…)
            // 从键导出(as_command_char),走文本通道;方向/翻页键仍导航。
            if let Some(c) = key.kind.as_command_char() {
                return pipeline.step_char(c, env);
            }
        }

        let flags = pipeline.state_flags();

        match key.kind {
            // 3. Space / Enter / Backspace:组合中是提交/强选/删除,idle 属于应用。
            KeyKind::Space => {
                if flags.contains(StateFlags::COMPOSING) {
                    pipeline.step_key(KeyKind::Space, env)
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }
            KeyKind::Enter => {
                if flags.contains(StateFlags::COMPOSING) {
                    pipeline.step_key(KeyKind::Enter, env)
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }
            KeyKind::Backspace => {
                if flags.contains(StateFlags::COMPOSING) {
                    pipeline.step_key(KeyKind::Backspace, env)
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }

            // 4. Escape:组合中取消(比旧的面板门控更宽 —— 无候选的组合也要能退),
            //    idle 透传给终端(vi 退回 normal 模式、取消半条命令…)。
            KeyKind::Escape => {
                if flags.contains(StateFlags::COMPOSING) {
                    pipeline.reset();
                    handled_empty_view()
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }

            // 5. Digit 1-9(Snippet 态已在上面 hoist 给状态机):面板展开时
            //    按**当前页内**序号选词(翻页后按 1 选的是新页的第一项,
            //    不是全列表第一项);否则透传(idle 的裸数字属于应用)。
            KeyKind::Digit(n) => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    let base = pipeline.panel.page.saturating_mul(pipeline.panel.page_size);
                    let idx = base + (n - 1) as usize;
                    if idx < pipeline.panel.items.len() {
                        pipeline.select(idx, env)
                    } else {
                        FamilyPipeline::passthrough_view()
                    }
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }

            // 6. 导航/翻页/移光标:仅候选面板展开时属于输入法,其余时候是
            //    应用自己的光标移动/翻页/`[` `]` `-` 字符。
            KeyKind::Up | KeyKind::Left | KeyKind::Tab => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    pipeline.move_highlight(-1);
                    pipeline.make_view()
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }
            KeyKind::Down | KeyKind::Right => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    pipeline.move_highlight(1);
                    pipeline.make_view()
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }
            KeyKind::PageUp | KeyKind::Minus => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    pipeline.change_page(-1);
                    pipeline.make_view()
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }
            KeyKind::PageDown | KeyKind::Plus | KeyKind::Equal => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    pipeline.change_page(1);
                    pipeline.make_view()
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }
            KeyKind::BracketLeft => {
                if flags.contains(StateFlags::PANEL_OPEN) && pipeline.comp.cursor > 0 {
                    pipeline.comp.cursor = pipeline.comp.cursor.saturating_sub(1);
                    pipeline.make_view()
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }
            KeyKind::BracketRight => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    let max = pipeline.comp.preedit.chars().count();
                    if pipeline.comp.cursor < max {
                        pipeline.comp.cursor += 1;
                    }
                    pipeline.make_view()
                } else {
                    FamilyPipeline::passthrough_view()
                }
            }

            // 7. 引擎不解释的键 —— Home/End/Delete/Insert、裸修饰键、F 功能键、
            //    未识别 keysym:当前无输入法语义,属于应用。
            KeyKind::Home
            | KeyKind::End
            | KeyKind::Delete
            | KeyKind::Insert
            | KeyKind::Modifier
            | KeyKind::Function(_)
            | KeyKind::Other(_) => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    pipeline.reset();
                    handled_empty_view()
                } else {
                    FamilyPipeline::passthrough_view()
                }
            },

            // 8. 可打印字符:交给组合状态机的文本通道(idle 内部自分流:
            //    触发前缀进 Snippet,小写进 Pinyin,其余返回透传视图)。
            KeyKind::Char(c) => pipeline.step_char(c, env),
        }
    }
}

/// 键被消费但无内容可渲染(如 Escape 取消后的空屏)。
fn handled_empty_view() -> ImeView {
    let mut v = ImeView::empty();
    v.action = action::HANDLED;
    v
}

