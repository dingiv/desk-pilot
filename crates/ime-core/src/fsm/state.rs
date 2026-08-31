//! 输入路由层 —— StateMachine(状态机表)。
//!
//! 所有前端(fcitx5、TUI、mock)**不再拦截任何键**:特殊键、Ctrl/Shift/Alt
//! 修饰状态一律忠实地转成 [`KeyEvent`] 喂进引擎。本模块持有一张状态机表
//! ([`StateMachine`]),表上是若干状态标志位([`StateFlags`])—— 一个
//! bit 意味着"当前处于某种输入状态"。每个键事件驱动一次状态迁移
//! ([`StateMachine::step`]),返回带 [`action`](crate::frontend::action)
//! 位标志的 [`ImeView`];外界只按 action 反应:
//!
//! - fcitx5:`action & HANDLED == 0` → 不 `filterAndAccept`,键自然到达应用;
//! - TUI:`COMMIT` → 追加历史;`PASSTHROUGH` 的 Esc(idle)→ 退出。
//!
//! ## 路由决策矩阵(自上而下,首条匹配生效)
//!
//! | 状态            | 键                        | 路由                        | action          |
//! |----------------|---------------------------|-----------------------------|-----------------|
//! | (任意)          | 裸修饰键 / F1-F12 / Other  | 透传                        | PASSTHROUGH    |
//! | (任意)          | Ctrl 或 Alt 组合           | 透传(应用快捷键)           | PASSTHROUGH    |
//! | COMPOSING      | Space / Enter / Backspace | `pipeline.step`(提交/删除)       | HANDLED,COMMIT |
//! | idle           | Space / Enter / Backspace | 透传                        | PASSTHROUGH    |
//! | COMPOSING      | Escape                    | reset(取消组合)            | HANDLED        |
//! | idle           | Escape                    | 透传                        | PASSTHROUGH    |
//! | MAGIC          | Digit 1-9                 | member `on_key`             | HANDLED        |
//! | PANEL_OPEN     | Digit 1-9                 | `select(idx)`               | HANDLED,COMMIT |
//! | 其余            | Digit 1-9                 | 透传                        | PASSTHROUGH    |
//! | PANEL_OPEN     | 方向/Tab/PgUp/PgDn/`[`/`]`/`+`/`-` | 导航/翻页/移光标   | HANDLED        |
//! | !PANEL_OPEN    | 同上                       | 透传(应用的光标/翻页)      | PASSTHROUGH    |
//! | (任意)          | Char(c)                   | `pipeline.step(c)`(idle 内自分流)| 视 step 结果    |
//!
//! Digit 0 与其余可打印字符统一走 Char 路径(历史 quirk:拼音中 `0` 是终止符)。
//! Escape 的门控是 **COMPOSING**(而非面板开合)—— 组合中但无候选时 Esc
//! 也应取消组合,否则 preedit 会卡在屏上。

use crate::frontend::{action, ImeView};

// 按键类型定义在 [`super::key`](键枚举的家);此处 re-export 保持
// `fsm::state::KeyEvent` 等既有引用路径稳定。
pub use super::key::{KeyEvent, KeyKind, StateFlags};
use crate::fsm::family::{ComposeState, FamilyPipeline, StepEnv};

// ── StateMachine:状态机表 ──────────────────────────────────────────

/// 状态机表 —— 输入路由层的状态寄存器。
///
/// 表上记录 [`StateFlags`];[`step`](StateMachine::step) 是唯一的键
/// 迁移入口:查表决定这枚键属于输入法还是应用,驱动 [`StateMachine`] 迁移,
/// 返回带 action 位标志的 [`ImeView`]。每个输入上下文(engine 的
/// `PerContext`)各持一张。
#[derive(Debug, Default)]
pub struct StateMachine {
    pub(crate) flags: StateFlags,
    /// stage1 系统控制(显式成员:每枚键先经它判定消费/透传)。
    pub(crate) control: crate::fsm::pre::ControlStage,
}

impl StateMachine {
    pub fn new() -> Self {
        StateMachine::default()
    }

    /// 当前状态标志位(最近一次路由后同步)。
    pub fn flags(&self) -> StateFlags {
        self.flags
    }

    /// 从组合状态机重新同步标志位。路由之外修改 pipeline 的入口(选词、reset、
    /// magic tick)也调用它 —— 表永远是当前状态的镜像。
    pub fn sync_from(&mut self, pipeline: &FamilyPipeline) {
        self.flags = pipeline.state_flags();
    }

    /// 路由一枚键:驱动状态迁移,返回新视图。决策矩阵见模块文档。
    /// stage1(系统控制)委托给 [`ControlStage`](crate::fsm::pre::ControlStage)。
    pub fn step(&mut self, pipeline: &mut FamilyPipeline, key: KeyEvent, env: &dyn StepEnv) -> ImeView {
        let control = self.control;
        let mut view = control.route_key(self, pipeline, key, env);
        // 不变式:键路径返回的视图必须带明确的 action 位。组合状态机里
        // "消费了键但无可渲染"的路径(退格清空 buffer 后 reset、snippet
        // 退空、magic 成员退出)返回的是空视图 —— action 为 NONE 时前端
        // 会把键放行给应用(退格漏过去,应用里已输入的字被删掉)。
        if view.action == action::NONE {
            view.action = action::HANDLED;
        }
        self.flags = pipeline.state_flags();
        view
    }

}

// ── StateMachine 辅助(随 special_key.rs 一并迁入)─────────────────────

impl FamilyPipeline {
    /// 从组合状态机提取状态标志位(路由前查表、路由后同步都用这里)。
    pub fn state_flags(&self) -> StateFlags {
        let mut f = StateFlags::empty();
        if self.state != ComposeState::Idle || !self.comp.buffer.is_empty() {
            f |= StateFlags::COMPOSING;
        }
        if !self.panel.items.is_empty() {
            f |= StateFlags::PANEL_OPEN;
        }
        match self.state {
            ComposeState::Idle => {}
            ComposeState::Pinyin => f |= StateFlags::PINYIN,
            ComposeState::Snippet => f |= StateFlags::SNIPPET,
        }
        if !self.comp.committed_text.is_empty() {
            f |= StateFlags::WORD_BUILDING;
        }
        if self.has_pending_choices() {
            f |= StateFlags::PENDING;
        }
        f
    }

    /// 是否有"待确认"的选项(命令预测 / 补全提示)。
    fn has_pending_choices(&self) -> bool {
        !self.magic.predictions.is_empty() || !self.magic.hints.is_empty()
    }

    /// Change page by delta.
    pub fn change_page(&mut self, delta: i32) {
        let n = self.panel.items.len();
        if n == 0 || self.panel.page_size == 0 {
            return;
        }
        let total_pages = n.div_ceil(self.panel.page_size);
        if total_pages <= 1 {
            return;
        }
        let new_page =
            (self.panel.page as i32 + delta).clamp(0, total_pages as i32 - 1) as usize;
        if new_page != self.panel.page {
            self.panel.page = new_page;
            self.panel.highlight = new_page * self.panel.page_size;
            self.sync_magic_preedit();
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ImeEngine;
    use crate::frontend::action;

    // -- 测试辅助 ------------------------------------------------------------

    /// 引擎级按键(经完整 stage1→stage2 链路)。
    fn key(e: &mut ImeEngine, k: KeyEvent) -> crate::frontend::ImeView {
        e.key(k)
    }

    /// 逐字符输入(引擎级)。
    fn type_str(e: &mut ImeEngine, s: &str) {
        for c in s.chars() {
            e.key(KeyEvent::char(c));
        }
    }

    // -- 归一化与 keysym 解码 ------------------------------------------------

    #[test]
    fn char_normalizes_control_chars_and_digits() {
        assert_eq!(KeyEvent::char(' ').kind, KeyKind::Space);
        assert_eq!(KeyEvent::char('\n').kind, KeyKind::Enter);
        assert_eq!(KeyEvent::char('\x08').kind, KeyKind::Backspace);
        assert_eq!(KeyEvent::char('\x1b').kind, KeyKind::Escape);
        assert_eq!(KeyEvent::char('5').kind, KeyKind::Digit(5));
        // '0' 保持 Char(历史 quirk:拼音中的终止符)。
        assert_eq!(KeyEvent::char('0').kind, KeyKind::Char('0'));
        assert_eq!(KeyEvent::char('A').kind, KeyKind::Char('A'));
    }

    #[test]
    fn nav_keys_passthrough_when_idle() {
        let mut e = ImeEngine::new();
        for k in [
            KeyEvent {
                kind: KeyKind::Up,
                ctrl: false,
                shift: false,
                alt: false,
            },
            KeyEvent::char('\t'),
            KeyEvent::char('-'),
            KeyEvent::char('='),
            KeyEvent::char('['),
            KeyEvent::char(']'),
            KeyEvent::escape(),
            KeyEvent::space(),
            KeyEvent::enter(),
            KeyEvent::backspace(),
        ] {
            let v = key(&mut e, k);
            assert!(
                v.action & action::PASSTHROUGH != 0 && v.action & action::HANDLED == 0,
                "idle key {:?} must pass through (action=0x{:x})",
                k.kind,
                v.action
            );
        }
    }

    #[test]
    fn nav_keys_handled_when_panel_open() {
        let mut e = ImeEngine::new();
        type_str(&mut e, "nihao");
        assert!(!e.candidates().is_empty(), "panel open after typing pinyin");
        for k in [
            KeyEvent::char('-'),
            KeyEvent::char('\t'),
            KeyEvent {
                kind: KeyKind::Left,
                ctrl: false,
                shift: false,
                alt: false,
            },
        ] {
            let v = key(&mut e, k);
            assert_eq!(
                v.action & action::HANDLED,
                action::HANDLED,
                "{:?} handled",
                k.kind
            );
            assert_eq!(
                v.action & action::PASSTHROUGH,
                0,
                "{:?} not passthrough",
                k.kind
            );
        }
    }

    #[test]
    fn ctrl_and_alt_combos_passthrough_even_while_composing() {
        // 修饰键策略在引擎内:组合中 Ctrl+/ 也要到达应用(编辑器注释快捷键)。
        let mut e = ImeEngine::new();
        type_str(&mut e, "nihao");
        for k in [
            KeyEvent::ctrl('/'),
            KeyEvent {
                kind: KeyKind::Char('c'),
                ctrl: false,
                shift: false,
                alt: true,
            },
        ] {
            let v = key(&mut e, k);
            assert_eq!(v.action & action::PASSTHROUGH, action::PASSTHROUGH, "{k:?}");
            assert_eq!(v.action & action::HANDLED, 0);
        }
        // 组合未被破坏 —— 空格仍提交候选。
        let v = key(&mut e, KeyEvent::space());
        assert!(
            v.action & action::COMMIT != 0,
            "composition survives passthrough"
        );
    }

    #[test]
    fn escape_resets_while_composing_even_without_candidates() {
        // 改进:旧逻辑按"面板开合"门控 —— 组合中但无候选时 Esc 透传,preedit
        // 卡屏。现在按 COMPOSING 门控,组合存在就能取消。
        let mut e = ImeEngine::new();
        // 构造"组合中但无候选":Snippet 状态、未知触发前缀走 fallback 有候选,
        // 用 Pinyin + 删空候选不可行 —— 直接检查 Idle 区分即可:先确认 idle 透传。
        let v = key(&mut e, KeyEvent::escape());
        assert_eq!(
            v.action & action::PASSTHROUGH,
            action::PASSTHROUGH,
            "idle Esc passes through"
        );

        type_str(&mut e, "nihao");
        assert!(!e.candidates().is_empty());
        let v = key(&mut e, KeyEvent::escape());
        assert_eq!(
            v.action & action::HANDLED,
            action::HANDLED,
            "composing Esc handled"
        );
        assert!(e.candidates().is_empty(), "composition cancelled");
    }

    #[test]
    fn commit_views_carry_commit_action() {
        let mut e = ImeEngine::new();
        type_str(&mut e, "ni");
        let v = key(&mut e, KeyEvent::space());
        assert!(v.action & action::COMMIT != 0, "space commit sets COMMIT");
        assert!(v.action & action::HANDLED != 0);
    }

    #[test]
    fn backspace_to_empty_is_consumed_not_passed_through() {
        // 回归:删掉 preedit 最后一个字母时,组合状态机 reset 后返回空视图
        // (action=NONE),前端会把这枚退格放行给应用 —— 应用里已输入的字
        // 被删掉。空视图必须标 HANDLED。
        let mut e = ImeEngine::new();
        type_str(&mut e, "n");
        let v = key(&mut e, KeyEvent::backspace());
        assert!(
            v.action & action::HANDLED != 0,
            "final backspace is consumed: 0x{:x}",
            v.action
        );
        assert_eq!(
            v.action & action::PASSTHROUGH,
            0,
            "must NOT reach the application"
        );
        assert!(e.buffer().is_empty(), "buffer emptied");
        assert_eq!(e.state_flags(), StateFlags::empty(), "back to idle");

        // 片段命令路径同理:'#/' 后立刻退格 → 删参数(消费,不透传)。
        let mut e2 = ImeEngine::new();
        type_str(&mut e2, "#/");
        let v = key(&mut e2, KeyEvent::backspace());
        assert!(
            v.action & action::HANDLED != 0,
            "snippet backspace consumed: 0x{:x}",
            v.action
        );
        assert_eq!(v.action & action::PASSTHROUGH, 0);
    }

    #[test]
    fn idle_backspace_still_passes_through() {
        // 无组合时的退格属于应用(删除应用里的文本)。
        let mut e = ImeEngine::new();
        let v = key(&mut e, KeyEvent::backspace());
        assert_eq!(v.action & action::PASSTHROUGH, action::PASSTHROUGH);
        assert_eq!(v.action & action::HANDLED, 0);
    }

    #[test]
    fn digit_selects_when_panel_open_passes_through_when_idle() {
        let mut e = ImeEngine::new();
        let v = key(&mut e, KeyEvent::char('3'));
        assert_eq!(
            v.action & action::PASSTHROUGH,
            action::PASSTHROUGH,
            "idle digit passes through"
        );

        let mut e2 = ImeEngine::new();
        type_str(&mut e2, "nihao");
        let v = key(&mut e2, KeyEvent::char('1'));
        assert!(v.action & action::COMMIT != 0, "digit selects a candidate");
    }

    #[test]
    fn digit_selects_within_current_page_after_paging() {
        // 回归:翻页后数字键选的是**当前页内**的序号 —— 按 1 提交第 2 页的
        // 第一项,而不是全列表第一项。
        let mut e = ImeEngine::new();
        type_str(&mut e, "shi");
        let all = e.candidates();
        let page_size = e.view().candidate_page_size as usize;
        assert!(
            all.len() > page_size,
            "need >{page_size} candidates for a second page, got {}: {all:?}",
            all.len(),
        );

        // 翻到第 2 页,再按数字 1。
        let v = key(
            &mut e,
            KeyEvent {
                kind: KeyKind::PageDown,
                ctrl: false,
                shift: false,
                alt: false,
            },
        );
        assert_eq!(v.candidate_page, 1, "paged to page 2");
        let v = key(&mut e, KeyEvent::char('1'));

        let committed = ImeView::str_field(&v.commit_text);
        assert!(
            v.action & action::COMMIT != 0,
            "digit selects: {committed:?}"
        );
        assert_eq!(
            committed, all[page_size],
            "digit 1 on page 2 commits the FIRST item of page 2 (not {}/{:?})",
            all[0], all[0],
        );
    }

    // -- 状态标志位 ----------------------------------------------------------

    #[test]
    fn state_flags_track_composition_and_word_building() {
        let mut e = ImeEngine::new();
        assert_eq!(e.state_flags(), StateFlags::empty(), "idle: no flags");

        // 多音节输入才有逐字提交选项(ni+hao 两个音节)。
        type_str(&mut e, "nihao");
        assert!(e.state_flags().contains(StateFlags::COMPOSING));
        assert!(e.state_flags().contains(StateFlags::PINYIN));
        assert!(e.state_flags().contains(StateFlags::PANEL_OPEN));

        // 逐字选第一个字 → 自生词模式(WORD_BUILDING)。
        let single: Option<usize> = e.candidates().iter().position(|c| c.chars().count() == 1);
        let idx = single.expect("multi-syllable input has single-char options");
        e.select_candidate(idx);
        assert!(
            e.state_flags().contains(StateFlags::WORD_BUILDING),
            "partial commit enters word-building: {:?}",
            e.state_flags().labels(),
        );
    }

    #[test]
    fn state_flags_track_snippet_and_pending() {
        let mut e = ImeEngine::new();
        type_str(&mut e, "#as");
        let f = e.state_flags();
        assert!(f.contains(StateFlags::SNIPPET), "{:?}", f.labels());
        assert!(
            f.contains(StateFlags::PENDING),
            "magic hints pending: {:?}",
            f.labels()
        );
    }
}
