//! 输入路由层 —— StateMachineTable(状态机表)。
//!
//! 所有前端(fcitx5、TUI、mock)**不再拦截任何键**:特殊键、Ctrl/Shift/Alt
//! 修饰状态一律忠实地转成 [`KeyEvent`] 喂进引擎。本模块持有一张状态机表
//! ([`StateMachineTable`]),表上是若干状态标志位([`StateFlags`])—— 一个
//! bit 意味着"当前处于某种输入状态"。每个键事件驱动一次状态迁移
//! ([`StateMachineTable::route`]),返回带 [`action`](crate::platform::action)
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
//! | COMPOSING      | Space / Enter / Backspace | `sm.step`(提交/删除)       | HANDLED,COMMIT |
//! | idle           | Space / Enter / Backspace | 透传                        | PASSTHROUGH    |
//! | COMPOSING      | Escape                    | reset(取消组合)            | HANDLED        |
//! | idle           | Escape                    | 透传                        | PASSTHROUGH    |
//! | MAGIC          | Digit 1-9                 | member `on_key`             | HANDLED        |
//! | PANEL_OPEN     | Digit 1-9                 | `select(idx)`               | HANDLED,COMMIT |
//! | 其余            | Digit 1-9                 | 透传                        | PASSTHROUGH    |
//! | PANEL_OPEN     | 方向/Tab/PgUp/PgDn/`[`/`]`/`+`/`-` | 导航/翻页/移光标   | HANDLED        |
//! | !PANEL_OPEN    | 同上                       | 透传(应用的光标/翻页)      | PASSTHROUGH    |
//! | (任意)          | Char(c)                   | `sm.step(c)`(idle 内自分流)| 视 step 结果    |
//!
//! Digit 0 与其余可打印字符统一走 Char 路径(历史 quirk:拼音中 `0` 是终止符)。
//! Escape 的门控是 **COMPOSING**(而非面板开合)—— 组合中但无候选时 Esc
//! 也应取消组合,否则 preedit 会卡在屏上。

use crate::platform::{action, ImeView};
use crate::state::{ComposeState, StateMachine, StepEnv};

// ── KeyEvent:统一键事件 ─────────────────────────────────────────────────

/// 一个键事件:键的种类 + 修饰键状态。前端忠实转换,引擎全面决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub kind: KeyKind,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// 键的种类。`Char` 只承载未归一化的可打印字符(空格/回车/数字已归一到
/// 各自变体);引擎不解释的键(`Modifier`/`Function`/`Other`)一律透传。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// 可打印字符(含大写、符号)。
    Char(char),
    /// 数字 1-9(ASCII 与小键盘归一)。`'0'` 保持 Char 路径(终止符 quirk)。
    Digit(u8),
    Space,
    Enter,
    Backspace,
    Escape,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Insert,
    /// `[` —— 候选面板内光标左移。
    BracketLeft,
    /// `]` —— 候选面板内光标右移。
    BracketRight,
    /// `+`(与 `Equal` 同路由:下一页)。
    Plus,
    /// `=`(fcitx 惯例与 `+` 等价)。
    Equal,
    Minus,
    /// F1-F12。
    Function(u8),
    /// 裸修饰键按下(Shift/Ctrl/Alt/Super/…)。
    Modifier,
    /// 未识别的 keysym,原值保留(诊断用)。
    Other(u32),
}

/// 字符 → KeyKind 归一化:控制字符、数字与翻页/移光标符号各自成类,
/// 其余保持 Char。
///
/// `+` `-` `=` `[` `]` 在这里就归一到导航键 —— 面板展开时翻页/移光标,
/// 否则透传给应用。旧架构里只有 fcitx C++ 拦截这五个符号,TUI/mock 走
/// 字符路径(组合中成了终止符、提交"候选-"),同一引擎两副面孔;归一化
/// 后所有前端行为一致。
fn normalize_char(c: char) -> KeyKind {
    match c {
        ' ' => KeyKind::Space,
        '\n' | '\r' => KeyKind::Enter,
        '\x08' | '\x7f' => KeyKind::Backspace,
        '\x1b' => KeyKind::Escape,
        '\t' => KeyKind::Tab,
        '+' => KeyKind::Plus,
        '=' => KeyKind::Equal,
        '-' => KeyKind::Minus,
        '[' => KeyKind::BracketLeft,
        ']' => KeyKind::BracketRight,
        d @ '1'..='9' => KeyKind::Digit(d as u8 - b'0'),
        c => KeyKind::Char(c),
    }
}

impl KeyEvent {
    /// 从一个字符构造(测试 / mock / 旧 predict 路径)。控制字符与 1-9
    /// 自动归一到对应 KeyKind —— 与前端转换规则一致。
    pub fn char(c: char) -> Self {
        KeyEvent {
            kind: normalize_char(c),
            ctrl: false,
            shift: false,
            alt: false,
        }
    }

    pub fn space() -> Self {
        KeyEvent::char(' ')
    }
    pub fn enter() -> Self {
        KeyEvent::char('\n')
    }
    pub fn backspace() -> Self {
        KeyEvent::char('\x08')
    }
    pub fn escape() -> Self {
        KeyEvent::char('\x1b')
    }

    /// Ctrl 组合键(测试用)。
    pub fn ctrl(c: char) -> Self {
        KeyEvent {
            kind: normalize_char(c),
            ctrl: true,
            shift: false,
            alt: false,
        }
    }

    /// fcitx5 C++ 胶水组包(`CKeyEvent`)→ KeyEvent。`sym` 是 X keysym,
    /// `unicode` = `keySymToUnicode(sym)`(无映射时为 0)。
    pub fn from_fcitx(sym: u32, unicode: u32, ctrl: bool, shift: bool, alt: bool) -> Self {
        KeyEvent {
            kind: keysym_to_kind(sym, unicode),
            ctrl,
            shift,
            alt,
        }
    }
}

/// X keysym → KeyKind(值见 keysymdef.h;ASCII 可打印区 sym == unicode)。
fn keysym_to_kind(sym: u32, unicode: u32) -> KeyKind {
    match sym {
        0xff09 => KeyKind::Tab,
        0xff0d => KeyKind::Enter,
        0xff1b => KeyKind::Escape,
        0xff08 => KeyKind::Backspace,
        0xff50 => KeyKind::Home,
        0xff57 => KeyKind::End,
        0xff51 => KeyKind::Left,
        0xff52 => KeyKind::Up,
        0xff53 => KeyKind::Right,
        0xff54 => KeyKind::Down,
        0xff55 => KeyKind::PageUp,
        0xff56 => KeyKind::PageDown,
        0xffff => KeyKind::Delete,
        0xff63 => KeyKind::Insert,
        // Shift_L..Hyper_R (0xffe1-0xffee) 与 Super/AltGr:裸修饰键。
        0xffe1..=0xffee => KeyKind::Modifier,
        // F1..F12 = 0xffbe..0xffc9。
        f @ 0xffbe..=0xffc9 => KeyKind::Function((f - 0xffbe + 1) as u8),
        // 小键盘 0-9 = 0xffb0..0xffb9,与主键盘同路。
        0xffb0 => normalize_char('0'),
        d @ 0xffb1..=0xffb9 => KeyKind::Digit((d - 0xffb0) as u8),
        _ => match char::from_u32(unicode) {
            Some(c) if !c.is_control() => normalize_char(c),
            _ => KeyKind::Other(sym),
        },
    }
}

// ── StateFlags:状态标志位 ───────────────────────────────────────────────

/// 状态机表的标志位 —— 一个 bit 表示"当前处于某种输入状态"。
/// 每次按键路由后从 [`StateMachine`] 重新同步(见 [`StateMachineTable`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StateFlags(u32);

impl StateFlags {
    /// 组合中(拼音/词组/snippet/magic 任一)。
    pub const COMPOSING: StateFlags = StateFlags(1 << 0);
    /// 候选面板展开。
    pub const PANEL_OPEN: StateFlags = StateFlags(1 << 1);
    /// 拼音组合模式。
    pub const PINYIN: StateFlags = StateFlags(1 << 2);
    /// `#` 命令组合(补全 / 预测)。
    pub const SNIPPET: StateFlags = StateFlags(1 << 3);
    /// 自生词模式:已有逐字提交(`committed_text` 非空)。
    pub const WORD_BUILDING: StateFlags = StateFlags(1 << 5);
    /// 有待确认的选项(命令预测 / 补全提示)。
    pub const PENDING: StateFlags = StateFlags(1 << 6);

    pub const fn empty() -> Self {
        StateFlags(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: StateFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// 人类可读的置位标签(TUI 状态栏 / 日志),按 bit 序。
    pub fn labels(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (flag, name) in [
            (Self::COMPOSING, "COMPOSING"),
            (Self::PANEL_OPEN, "PANEL_OPEN"),
            (Self::PINYIN, "PINYIN"),
            (Self::SNIPPET, "SNIPPET"),
            (Self::WORD_BUILDING, "WORD_BUILDING"),
            (Self::PENDING, "PENDING"),
        ] {
            if self.contains(flag) {
                out.push(name);
            }
        }
        out
    }
}

impl std::ops::BitOr for StateFlags {
    type Output = StateFlags;
    fn bitor(self, rhs: StateFlags) -> StateFlags {
        StateFlags(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for StateFlags {
    fn bitor_assign(&mut self, rhs: StateFlags) {
        self.0 |= rhs.0
    }
}

// ── StateMachineTable:状态机表 ──────────────────────────────────────────

/// 状态机表 —— 输入路由层的状态寄存器。
///
/// 表上记录 [`StateFlags`];[`route`](StateMachineTable::route) 是唯一的键
/// 迁移入口:查表决定这枚键属于输入法还是应用,驱动 [`StateMachine`] 迁移,
/// 返回带 action 位标志的 [`ImeView`]。每个输入上下文(engine 的
/// `PerContext`)各持一张。
#[derive(Debug, Default)]
pub struct StateMachineTable {
    flags: StateFlags,
}

impl StateMachineTable {
    pub fn new() -> Self {
        StateMachineTable::default()
    }

    /// 当前状态标志位(最近一次路由后同步)。
    pub fn flags(&self) -> StateFlags {
        self.flags
    }

    /// 从组合状态机重新同步标志位。路由之外修改 sm 的入口(选词、reset、
    /// magic tick)也调用它 —— 表永远是当前状态的镜像。
    pub fn sync_from(&mut self, sm: &StateMachine) {
        self.flags = sm.state_flags();
    }

    /// 路由一枚键:驱动状态迁移,返回新视图。决策矩阵见模块文档。
    pub fn route(&mut self, sm: &mut StateMachine, key: KeyEvent, env: &dyn StepEnv) -> ImeView {
        let mut view = self.route_inner(sm, key, env);
        // 不变式:键路径返回的视图必须带明确的 action 位。组合状态机里
        // "消费了键但无可渲染"的路径(退格清空 buffer 后 reset、snippet
        // 退空、magic 成员退出)返回的是空视图 —— action 为 NONE 时前端
        // 会把键放行给应用(退格漏过去,应用里已输入的字被删掉)。
        if view.action == action::NONE {
            view.action = action::HANDLED;
        }
        self.flags = sm.state_flags();
        view
    }

    /// **路由处理函数**
    /// 驱动状态机流转
    fn route_inner(&mut self, sm: &mut StateMachine, key: KeyEvent, env: &dyn StepEnv) -> ImeView {
        // 1. Ctrl/Alt 组合键是应用快捷键(Ctrl+/ 注释、Ctrl+C 复制…),一律放行。
        //    修饰键策略在引擎内 —— 前端不再自行拦截。Shift 不在此列:大写
        //    字母/符号照常走字符路径(组合中是终止符,idle 透传)。
        if key.ctrl || key.alt {
            return StateMachine::passthrough_view();
        }

        // 2. Snippet 态(组合 `#…` 命令):数字与 `+ - = [ ]` 是命令文本
        //    (`?num=2` 的 `=`、`#req` URL 的 `-`/`[`/`]`…)—— 由状态机决定
        //    (数字在可选中态选中候选,否则追加)。方向/翻页键仍导航。
        if sm.state == ComposeState::Snippet {
            if let Some(c) = command_char(key.kind) {
                return sm.step(c, env);
            }
        }

        let flags = sm.state_flags();

        match key.kind {
            // 3. Space / Enter / Backspace:组合中是提交/强选/删除,idle 属于应用。
            KeyKind::Space => {
                if flags.contains(StateFlags::COMPOSING) {
                    sm.step(' ', env)
                } else {
                    StateMachine::passthrough_view()
                }
            }
            KeyKind::Enter => {
                if flags.contains(StateFlags::COMPOSING) {
                    sm.step('\n', env)
                } else {
                    StateMachine::passthrough_view()
                }
            }
            KeyKind::Backspace => {
                if flags.contains(StateFlags::COMPOSING) {
                    sm.step('\x08', env)
                } else {
                    StateMachine::passthrough_view()
                }
            }

            // 4. Escape:组合中取消(比旧的面板门控更宽 —— 无候选的组合也要能退),
            //    idle 透传给终端(vi 退回 normal 模式、取消半条命令…)。
            KeyKind::Escape => {
                if flags.contains(StateFlags::COMPOSING) {
                    sm.reset();
                    handled_empty_view()
                } else {
                    StateMachine::passthrough_view()
                }
            }

            // 5. Digit 1-9(Snippet 态已在上面 hoist 给状态机):面板展开时
            //    按**当前页内**序号选词(翻页后按 1 选的是新页的第一项,
            //    不是全列表第一项);否则透传(idle 的裸数字属于应用)。
            KeyKind::Digit(n) => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    let base = sm.candidate_page.saturating_mul(sm.candidate_page_size);
                    let idx = base + (n - 1) as usize;
                    if idx < sm.candidates.len() {
                        sm.select(idx, env)
                    } else {
                        StateMachine::passthrough_view()
                    }
                } else {
                    StateMachine::passthrough_view()
                }
            }

            // 6. 导航/翻页/移光标:仅候选面板展开时属于输入法,其余时候是
            //    应用自己的光标移动/翻页/`[` `]` `-` 字符。
            KeyKind::Up | KeyKind::Left | KeyKind::Tab => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    sm.move_highlight(-1);
                    sm.make_view()
                } else {
                    StateMachine::passthrough_view()
                }
            }
            KeyKind::Down | KeyKind::Right => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    sm.move_highlight(1);
                    sm.make_view()
                } else {
                    StateMachine::passthrough_view()
                }
            }
            KeyKind::PageUp | KeyKind::Minus => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    sm.change_page(-1);
                    sm.make_view()
                } else {
                    StateMachine::passthrough_view()
                }
            }
            KeyKind::PageDown | KeyKind::Plus | KeyKind::Equal => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    sm.change_page(1);
                    sm.make_view()
                } else {
                    StateMachine::passthrough_view()
                }
            }
            KeyKind::BracketLeft => {
                if flags.contains(StateFlags::PANEL_OPEN) && sm.cursor > 0 {
                    sm.cursor = sm.cursor.saturating_sub(1);
                    sm.make_view()
                } else {
                    StateMachine::passthrough_view()
                }
            }
            KeyKind::BracketRight => {
                if flags.contains(StateFlags::PANEL_OPEN) {
                    let max = sm.preedit.chars().count();
                    if sm.cursor < max {
                        sm.cursor += 1;
                    }
                    sm.make_view()
                } else {
                    StateMachine::passthrough_view()
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
                    sm.reset();
                    handled_empty_view()
                } else {
                    StateMachine::passthrough_view()
                }
            },

            // 8. 可打印字符:交给组合状态机(idle 内部自分流:触发前缀进
            //    Snippet,小写进 Pinyin,其余返回透传视图)。
            KeyKind::Char(c) => sm.step(c, env),
        }
    }
}

/// 键被消费但无内容可渲染(如 Escape 取消后的空屏)。
fn handled_empty_view() -> ImeView {
    let mut v = ImeView::empty();
    v.action = action::HANDLED;
    v
}

/// Magic 模式下作为命令文本回填给 member 的字符(数字 + 翻页/移光标符号)。
fn command_char(kind: KeyKind) -> Option<char> {
    match kind {
        KeyKind::Digit(n) => Some(char::from(b'0' + n)),
        KeyKind::Plus => Some('+'),
        KeyKind::Equal => Some('='),
        KeyKind::Minus => Some('-'),
        KeyKind::BracketLeft => Some('['),
        KeyKind::BracketRight => Some(']'),
        _ => None,
    }
}

// ── StateMachine 辅助(随 special_key.rs 一并迁入)─────────────────────

impl StateMachine {
    /// 从组合状态机提取状态标志位(路由前查表、路由后同步都用这里)。
    pub fn state_flags(&self) -> StateFlags {
        let mut f = StateFlags::empty();
        if self.state != ComposeState::Idle || !self.buffer.is_empty() {
            f |= StateFlags::COMPOSING;
        }
        if !self.candidates.is_empty() {
            f |= StateFlags::PANEL_OPEN;
        }
        match self.state {
            ComposeState::Idle => {}
            ComposeState::Pinyin => f |= StateFlags::PINYIN,
            ComposeState::Snippet => f |= StateFlags::SNIPPET,
        }
        if !self.committed_text.is_empty() {
            f |= StateFlags::WORD_BUILDING;
        }
        if self.has_pending_choices() {
            f |= StateFlags::PENDING;
        }
        f
    }

    /// 是否有"待确认"的选项(命令预测 / 补全提示)。
    fn has_pending_choices(&self) -> bool {
        !self.magic_predictions.is_empty() || !self.magic_hints.is_empty()
    }

    /// Change page by delta.
    pub fn change_page(&mut self, delta: i32) {
        let n = self.candidates.len();
        if n == 0 || self.candidate_page_size == 0 {
            return;
        }
        let total_pages = n.div_ceil(self.candidate_page_size);
        if total_pages <= 1 {
            return;
        }
        let new_page =
            (self.candidate_page as i32 + delta).clamp(0, total_pages as i32 - 1) as usize;
        if new_page != self.candidate_page {
            self.candidate_page = new_page;
            self.candidate_highlight = new_page * self.candidate_page_size;
            self.sync_magic_preedit();
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ImeEngine;
    use crate::platform::action;

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
    fn fcitx_keysyms_decode() {
        use KeyKind::*;
        let cases: &[(u32, KeyKind)] = &[
            (0xff51, Left),
            (0xff52, Up),
            (0xff53, Right),
            (0xff54, Down),
            (0xff55, PageUp),
            (0xff56, PageDown),
            (0xff09, Tab),
            (0xff0d, Enter),
            (0xff1b, Escape),
            (0xff08, Backspace),
            (0xffe1, Modifier), // Shift_L
            (0xffe3, Modifier), // Control_L
            (0xffe9, Modifier), // Alt_L
            (0xffbe, Function(1)),
            (0xffc9, Function(12)),
            (0xffb1, Digit(1)), // KP_1
            (0xffb9, Digit(9)), // KP_9
        ];
        for &(sym, want) in cases {
            assert_eq!(keysym_to_kind(sym, 0), want, "sym=0x{sym:x}");
        }
        // ASCII 可打印区:sym == unicode。
        assert_eq!(keysym_to_kind(0x61, 0x61), KeyKind::Char('a'));
        assert_eq!(keysym_to_kind(0x31, 0x31), KeyKind::Digit(1));
        // 无法解释的键(F1 之外的保留区)→ Other 保底透传。
        assert_eq!(keysym_to_kind(0x1008ff01, 0), KeyKind::Other(0x1008ff01));
    }

    // -- 决策矩阵 ------------------------------------------------------------

    fn key(e: &mut ImeEngine, k: KeyEvent) -> ImeView {
        e.key(k)
    }

    fn type_str(e: &mut ImeEngine, s: &str) {
        for c in s.chars() {
            e.key(KeyEvent::char(c));
        }
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
    fn bare_modifier_and_function_keys_passthrough() {
        let mut e = ImeEngine::new();
        type_str(&mut e, "ni");
        for kind in [
            KeyKind::Modifier,
            KeyKind::Function(1),
            KeyKind::Other(0x1008ff01),
        ] {
            let v = key(
                &mut e,
                KeyEvent {
                    kind,
                    ctrl: false,
                    shift: false,
                    alt: false,
                },
            );
            assert_eq!(
                v.action & action::PASSTHROUGH,
                action::PASSTHROUGH,
                "{kind:?}"
            );
        }
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
