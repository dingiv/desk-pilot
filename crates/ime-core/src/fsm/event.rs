//! event — 统一事件模型(round12)。
//!
//! 前端能对输入法做的一切动作统一封装为 [`ImeEvent`],一律从 stage1
//! (系统控制)进:stage1 区分三类事件并决定去向 ——
//!
//! - [`ImeEvent::Key`](KeyEvent):键盘事件,走路由矩阵(输入法消费还是透传);
//! - [`ImeEvent::Control`]:控制事件(编程式选词 / 翻页 / 复位等,
//!   非键盘来源:鼠标点选、FFI、前端设置),**必被输入法消费**,
//!   stage1 直达 stage2 对应门面;
//! - [`ImeEvent::Async`]:异步事件(命令会话轮询 tick 等),同样交
//!   stage2 门面驱动。
//!
//! 壳(engine)与前端不再自行挑选 stage2 方法 —— 动作即事件,
//! 事件即 stage1 的唯一入口。

use crate::frontend::ImeView;

use super::control::SessionState;
use super::key::KeyEvent;
use super::pre::ControlStage;
use super::family_prediction::StepEnv;
/// 输入法统一事件(键盘 / 控制 / 异步)。
#[derive(Debug, Clone)]
pub enum ImeEvent {
    /// 键盘事件(路由矩阵裁决:输入法 or 应用)。
    Key(KeyEvent),
    /// 控制事件:编程式会话操作(必被消费,直达 stage2 门面)。
    Control(ControlEvent),
    /// 异步事件:无键驱动的会话推进。
    Async(AsyncEvent),
}

/// 控制事件:非键盘来源的会话操作(鼠标选词 / FFI / 前端设置)。
#[derive(Debug, Clone, Copy)]
pub enum ControlEvent {
    /// 全局序选词(鼠标点选)。
    Select(usize),
    /// 页内数字选词。
    SelectPageDigit(usize),
    /// 高亮移动(方向键编程式)。
    MoveHighlight(i32),
    /// 翻页。
    ChangePage(i32),
    /// preedit 光标移动。
    NudgeCursor(i32),
    /// 复位会话。
    Reset,
    /// 设置页大小。
    SetPageSize(usize),
}

/// 异步事件。
#[derive(Debug, Clone, Copy)]
pub enum AsyncEvent {
    /// 活跃魔法命令(`#asr` / `#req`)轮询推进。
    MagicTick,
}

impl ImeEvent {
    /// stage1 统一事件入口(由 [`ControlStage`] 裁决去向)。
    /// 返回:Key/Control 恒有视图;Async 的 MagicTick 无推进时为 None
    /// (前端据此跳过刷新)。flags 同步由 [`super::control::ControlPane`] 负责。
    pub fn handle(self, s: &mut SessionState, env: &dyn StepEnv) -> Option<ImeView> {
        match self {
            ImeEvent::Key(k) => Some(ControlStage.route_key(s, k, env)),
            ImeEvent::Control(c) => Some(c.handle(s, env)),
            ImeEvent::Async(a) => a.handle(s, env),
        }
    }
}

impl ControlEvent {
    /// stage1 裁决:控制事件必被输入法消费,直达 stage2 门面。
    /// 壳与前端不得绕过此处自行调用 stage2。
    pub fn handle(self, s: &mut SessionState, env: &dyn StepEnv) -> ImeView {
        match self {
            ControlEvent::Select(i) => s.select(i, env),
            ControlEvent::SelectPageDigit(d) => s.select_page_digit(d, env),
            ControlEvent::MoveHighlight(d) => {
                s.move_highlight(d);
                s.snapshot_view()
            }
            ControlEvent::ChangePage(d) => {
                s.change_page(d);
                s.snapshot_view()
            }
            ControlEvent::NudgeCursor(d) => {
                s.nudge_cursor(d);
                s.snapshot_view()
            }
            ControlEvent::Reset => {
                s.reset();
                s.snapshot_view()
            }
            ControlEvent::SetPageSize(n) => {
                s.set_page_size(n);
                s.snapshot_view()
            }
        }
    }
}

impl AsyncEvent {
    pub fn handle(self, s: &mut SessionState, env: &dyn StepEnv) -> Option<ImeView> {
        match self {
            AsyncEvent::MagicTick => s.magic_tick(env),
        }
    }
}
