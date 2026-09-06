//! Stage 1 系统控制(pre,round9 R3 自 state.rs 迁出):
//! 每枚键的第一站 —— 判定"输入法消费还是透传给应用"。
//!
//! [`ControlStage`] 是显式的 stage1 结构体(无内部状态,标志位寄存在
//! `ControlPane.flags`);它只做**系统级流转**(提交/选词/翻页/高亮/
//! 修饰键/命令文本 hoist),不产生预测 —— 字符预测一律交给
//! `ControlPane::step`(stage2 家族分发)。
//!
//! ## 路由决策矩阵(自上而下,首条匹配生效;权威版本,随实现)
//!
//! | 状态            | 键                        | 路由                        | action          |
//! |----------------|---------------------------|-----------------------------|-----------------|
//! | (任意)          | Ctrl 或 Alt 组合           | 透传(应用快捷键)           | PASSTHROUGH    |
//! | SNIPPET         | 数字/`+ - = [ ]`(as_command_char)| hoist 给文本通道     | 视 step 结果    |
//! | COMPOSING      | Space / Enter / Backspace | `SessionState::step_key`(提交/删除)| HANDLED,COMMIT |
//! | idle           | Space / Enter / Backspace | 透传                        | PASSTHROUGH    |
//! | COMPOSING      | Escape                    | reset(取消组合)            | HANDLED        |
//! | idle           | Escape                    | 透传                        | PASSTHROUGH    |
//! | PANEL_OPEN     | Digit 1-9                 | `select_page_digit`(页内序号)| HANDLED,COMMIT |
//! | 其余            | Digit 1-9                 | 透传                        | PASSTHROUGH    |
//! | PANEL_OPEN     | 方向/Tab/PgUp/PgDn/`[`/`]`/`+`/`-` | 导航/翻页/`nudge_cursor` | HANDLED  |
//! | !PANEL_OPEN    | 同上                       | 透传(应用的光标/翻页)      | PASSTHROUGH    |
//! | PANEL_OPEN     | 引擎不解释的键             | reset(收起面板)            | HANDLED        |
//! | (任意)          | Char(c)                   | `SessionState::step_char`(idle 内自分流)| 视 step 结果 |
//!
//! Digit 0 与其余可打印字符统一走 Char 路径(历史 quirk:拼音中 `0` 是终止符)。
//! Escape 的门控是 **COMPOSING**(而非面板开合)—— 组合中但无候选时 Esc
//! 也应取消组合,否则 preedit 会卡在屏上。

use super::post::passthrough_view;
use super::control::SessionState;
use super::control::{KeyEvent, KeyKind, StateFlags};
use crate::frontend::{action, ImeView};
use crate::fsm::family_prediction::{ComposeState, StepEnv};

/// Stage 1 系统控制(显式结构体:stage1 是管线的一员,不是散落的 match)。
/// 无内部状态(零大小,Copy —— 表按值取用),行为即结构。
///
/// 边界规则(round11):stage1 不触碰 stage2 聚合体内部字段(panel/comp
/// 对 stage1 不可见)—— 选词/翻页/高亮/移光标一律调 `ControlPane`
/// 的门面方法;action 归一化(NONE→HANDLED)与 flags 同步由
/// [`ControlPane::step`](crate::fsm::control::ControlPane::step) 统一收口,
/// 本模块只返回裸视图。
#[derive(Debug, Default, Clone, Copy)]
pub struct ControlStage;

/// 路由层编排(round12):交付件就绪(None = 面板已就绪,魔法路径/无候选
/// 直出视图)时传给 stage3 纯函数,回执交回 stage2 落位 —— stage2 不调
/// stage3。(计划的 Stage2Result 两态枚举收敛为 Option:Done 变体无构造点。)
pub(crate) fn resolve(
    s: &mut SessionState,
    request: Option<crate::fsm::post::PostRequest>,
    env: &dyn StepEnv,
) -> ImeView {
    match request {
        None => s.make_view(),
        Some(req) => {
            let outcome = crate::fsm::post::postprocess(req, env);
            s.apply_post_outcome(outcome)
        }
    }
}

impl ControlStage {
    /// 路由一枚键:系统键就地处理,字符键交给 stage2。
    pub fn route_key(
        &self,
        s: &mut SessionState,
        key: KeyEvent,
        env: &dyn StepEnv,
    ) -> ImeView {
        self.route_inner(s, key, env)
    }

    fn route_inner(
        &self,
        s: &mut SessionState,
        key: KeyEvent,
        env: &dyn StepEnv,
    ) -> ImeView {
        // 1. Ctrl/Alt 组合键是应用快捷键(Ctrl+/ 注释、Ctrl+C 复制…),一律放行。
        //    修饰键策略在引擎内 —— 前端不再自行拦截。Shift 不在此列:大写
        //    字母/符号照常走字符路径(组合中是终止符,idle 透传)。
        if key.ctrl || key.alt {
            return passthrough_view();
        }

        // 2. Snippet 态(组合 `#…` 命令):数字与 `+ - = [ ]` 是命令文本
        //    (`?num=2` 的 `=`、`#req` URL 的 `-`/`[`/`]`…)—— 由状态机决定
        //    (数字在可选中态选中候选,否则追加)。方向/翻页键仍导航。
        if s.state == ComposeState::Snippet {
            // 命令文本字符(`?num=2` 的数字、`#req` URL 的 `-`/`[`/`]`…)
            // 从键导出(as_command_char),走文本通道;方向/翻页键仍导航。
            if let Some(c) = key.kind.as_command_char() {
                return s.step_char(c, env);
            }
        }

        let flags = s.state_flags();

        match key.kind {
            // 3. Space / Enter / Backspace:组合中是提交/强选/删除,idle 属于应用。
            KeyKind::Space => {
                if flags.contains(StateFlags::COMPOSING) {
                    s.step_key(KeyKind::Space, env)
                } else {
                    passthrough_view()
                }
            }
            KeyKind::Enter => {
                if flags.contains(StateFlags::COMPOSING) {
                    s.step_key(KeyKind::Enter, env)
                } else {
                    passthrough_view()
                }
            }
            KeyKind::Backspace => {
                if flags.contains(StateFlags::COMPOSING) {
                    s.step_key(KeyKind::Backspace, env)
                } else {
                    passthrough_view()
                }
            }

            // 4. Escape:组合中取消(比旧的面板门控更宽 —— 无候选的组合也要能退),
            //    idle 透传给终端(vi 退回 normal 模式、取消半条命令…)。
            KeyKind::Escape => {
                if flags.contains(StateFlags::COMPOSING) {
                    s.reset();
                    handled_empty_view()
                } else {
                    passthrough_view()
                }
            }

            // 5. Digit 1-9(Snippet 态已在上面 hoist 给状态机):面板展开时
            //    按**当前页内**序号选词(翻页后按 1 选的是新页的第一项,
            //    不是全列表第一项);否则透传(idle 的裸数字属于应用)。
            //    页内序号→全局序→选中的换算属于 stage2 门面(select_page_digit)。
            KeyKind::Digit(n) => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    s.select_page_digit(n as usize, env)
                } else {
                    passthrough_view()
                }
            }

            // 6. 导航/翻页/移光标:仅候选面板展开时属于输入法,其余时候是
            //    应用自己的光标移动/翻页/`[` `]` `-` 字符。
            KeyKind::Up | KeyKind::Left | KeyKind::Tab => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    s.move_highlight(-1);
                    s.make_view()
                } else {
                    passthrough_view()
                }
            }
            KeyKind::Down | KeyKind::Right => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    s.move_highlight(1);
                    s.make_view()
                } else {
                    passthrough_view()
                }
            }
            KeyKind::PageUp | KeyKind::Minus => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    s.change_page(-1);
                    s.make_view()
                } else {
                    passthrough_view()
                }
            }
            KeyKind::PageDown | KeyKind::Plus | KeyKind::Equal => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    s.change_page(1);
                    s.make_view()
                } else {
                    passthrough_view()
                }
            }
            KeyKind::BracketLeft => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    s.nudge_cursor(-1);
                    s.make_view()
                } else {
                    passthrough_view()
                }
            }
            KeyKind::BracketRight => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    s.nudge_cursor(1);
                    s.make_view()
                } else {
                    passthrough_view()
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
                    s.reset();
                    handled_empty_view()
                } else {
                    passthrough_view()
                }
            },

            // 8. 可打印字符:交给组合状态机的文本通道(idle 内部自分流:
            //    触发前缀进 Snippet,小写进 Pinyin,其余返回透传视图)。
            KeyKind::Char(c) => s.step_char(c, env),
        }
    }
}

/// 键被消费但无内容可渲染(如 Escape 取消后的空屏)。
fn handled_empty_view() -> ImeView {
    let mut v = ImeView::empty();
    v.action = action::HANDLED;
    v
}

