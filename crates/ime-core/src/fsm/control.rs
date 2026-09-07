//! 输入路由层 —— ControlPane(系统控制)+ SessionState(会话数据)。
//!
//! 所有前端(fcitx5、TUI、mock)**不再拦截任何键**:特殊键、Ctrl/Shift/Alt
//! 修饰状态一律忠实地转成 [`KeyEvent`] 喂进引擎。本模块持有一张状态机表
//! ([`ControlPane`]),表上是若干状态标志位([`StateFlags`])—— 一个
//! bit 意味着"当前处于某种输入状态"。每个键事件驱动一次状态迁移
//! ([`StateMachine::step`]),返回带 [`action`](crate::frontend::action)
//! 位标志的 [`ImeView`];外界只按 action 反应:
//!
//! - fcitx5:`action & HANDLED == 0` → 不 `filterAndAccept`,键自然到达应用;
//! - TUI:`COMMIT` → 追加历史;`PASSTHROUGH` 的 Esc(idle)→ 退出。
//!
//! 路由决策矩阵(哪个键怎么分流)的权威版本在
//! [`fsm::pre`](crate::fsm::pre) 模块头 —— 随实现走。本模块只剩两件事:
//! 键迁移入口 [`StateMachine::step`](含 **action 归一化**与 flags 镜像
//! 两个收口职责)与状态标志位的查询。

use crate::frontend::{action, ImeView};

// 按键类型定义在 [`super::key`](键枚举的家);此处 re-export 保持
// `fsm::state::KeyEvent` 等既有引用路径稳定。
pub use super::key::{KeyEvent, KeyKind, StateFlags};
use super::family_prediction::{CandidatePanel, ComposeState, Composition};
use super::magic_flow::MagicSession;
use crate::fsm::event::ImeEvent;
use crate::fsm::family_prediction::StepEnv;

// ── SessionState / ControlPane ─────────────────────────────────────

/// 会话数据(round13):**纯数据**,无行为归属 —— 双路键处理
/// ([`super::family`] FamilyPrediction / [`super::magic`] MagicFlow)
/// 都只吃它,不依赖 [`ControlPane`]。
#[derive(Default)]
pub struct SessionState {
    /// 组合状态(Idle / Snippet / Pinyin)。
    pub state: ComposeState,
    /// 组合会话:原始键入/预测串/预编辑/光标/造词半成品。
    pub(crate) comp: Composition,
    /// 调试模式:候选词显示提供者与权重(`[score family/source]`)。
    pub candidate_meta_enabled: bool,
    /// 候选面板:items/meta/partial 同源同序 + 高亮/分页。
    pub(crate) panel: CandidatePanel,
    /// Short-term input context — accumulates recently committed text
    /// (上下文构建:预测前经 `scorer.collect` 注入各家族,家族据此做
    /// 上下文感知)。
    pub context: crate::family::InputContext,
    /// 魔法命令会话:snippet 态的补全提示 / 预测选项 / live 命令实例。
    pub(crate) magic: MagicSession,
    /// 单词本引用(round14:存储归持久化模块所有,双路/后处理共享;
    /// 引擎装配时分发 Arc 克隆)。
    pub wordbook: std::sync::Arc<crate::store::wordbook::WordBook>,
    /// 所属输入上下文(引擎 `with_ctx` 每次操作前设置)。魔法命令成员用它
    /// 把异步工作事件发到正确的 ctx 并 refresh 对应上下文。
    pub ctx: usize,
    /// 待结算学习回执(round14):stage2 提交路径产出,ControlPane 在
    /// 事件处理后统一交后处理 `learn_commit` 结算。
    pub(crate) pending_learning: Vec<crate::fsm::post::CommitReceipt>,
    /// 链式流控(round15):链式预测 × 魔法异步的刷新闸门 + 上游折叠缓存。
    /// `abc'#asr'#translate` 中语音段更新时,上游 abc 命中缓存不重算,
    /// 只有语音段之后的流水线在防抖/节流放行后重新预测。
    pub(crate) chain_flow: crate::fsm::chain::ChainFlow,
    /// 链式高亮锚点(round22):用户在拼音面板把高亮移到某候选(如
    /// yibu → 异步)后键入 `'` 开链 —— 分链那一刻捕获高亮词,供链式
    /// 上下文命令(#freq/up|down)作为操作对象;reset/提交后清空。
    pub(crate) chain_anchor: Option<String>,
}

/// 系统控制(round13,原 `StateMachine`):**分发三大事件类型** ——
/// ①普通键 → 第一路 FamilyPrediction(打分家族预测);
/// ②`#` 触发键 → 第二路 MagicFlow 同步数据流,异步信号(IoThread 注入)
///   → 第二路 MagicFlow 异步数据流;
/// ③系统控制事件(Control)提前拦截并返回(提交/选词/翻页/复位),
///   不进双路。
/// 处理完成后统一封装 [`ImeView`] 返回。每个输入上下文(engine 的
/// `Session`)各持一台。
#[derive(Default)]
pub struct ControlPane {
    /// 会话数据(双路共享;含 ctx 挂号)。
    pub(crate) session: SessionState,
    /// 状态标志位镜像(路由后同步;派生值,非状态本体)。
    pub(crate) flags: StateFlags,
}

impl ControlPane {
    pub fn new() -> Self {
        ControlPane::default()
    }

    /// Construct with a configurable candidate page size (default 7).
    /// The engine passes `swift-ime.yaml → input.page_size` here, plus the
    /// shared wordbook reference(所有权在持久化模块)。
    pub fn with_page_size(
        page_size: u32,
        wordbook: std::sync::Arc<crate::store::wordbook::WordBook>,
    ) -> Self {
        ControlPane {
            session: SessionState::with_page_size(page_size, wordbook),
            ..ControlPane::default()
        }
    }

    /// 当前状态标志位(最近一次路由后同步)。
    pub fn flags(&self) -> StateFlags {
        self.flags
    }

    /// 重新镜像标志位(路由之外的变更入口 —— 选词、reset、magic tick ——
    /// 也调用它)。flags 是派生值,状态本体就是自己身上的字段。
    pub fn sync_from(&mut self) {
        self.flags = self.session.state_flags();
    }

    /// 路由一枚键:驱动状态迁移,返回新视图。stage1 委托给
    /// [`ControlStage`](crate::fsm::pre::ControlStage);**action 归一化与
    /// flags 同步在此唯一收口**(round11:旧实现 route_key 尾部与这里
    /// 各一份完全相同的逻辑)。
    pub fn step(&mut self, key: KeyEvent, env: &dyn StepEnv) -> ImeView {
        self.handle_event(ImeEvent::Key(key), env)
            .unwrap_or_else(ImeView::empty)
    }

    /// 统一事件入口(round12):键盘 / 控制 / 异步三类事件,一律从 stage1
    /// 进。键路径同 [`Self::step`](action 归一化 + flags 同步);控制 /
    /// 异步路径直达 stage2 门面,路由外变更后重新镜像 flags。
    pub fn handle_event(&mut self, event: ImeEvent, env: &dyn StepEnv) -> Option<ImeView> {
        let is_key = matches!(event, ImeEvent::Key(_));
        let mut view = event.handle(&mut self.session, env)?;
        // 学习结算(round14):stage2 产出的回执统一交后处理分发。
        for receipt in self.session.pending_learning.drain(..) {
            crate::fsm::post::learn_commit(&receipt, &self.session.wordbook, env);
        }
        // 不变式:键路径返回的视图必须带明确的 action 位。组合状态机里
        // "消费了键但无可渲染"的路径(退格清空 buffer 后 reset、snippet
        // 退空、magic 成员退出)返回的是空视图 —— action 为 NONE 时前端
        // 会把键放行给应用(退格漏过去,应用里已输入的字被删掉)。
        if is_key && view.action == action::NONE {
            view.action = action::HANDLED;
        }
        if is_key {
            // 键路径与旧 step 一致:无条件镜像 flags。
            self.flags = self.session.state_flags();
        } else {
            // 路由之外的变更(选词/复位/tick)—— 重新镜像 flags。
            self.sync_from();
        }
        Some(view)
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
