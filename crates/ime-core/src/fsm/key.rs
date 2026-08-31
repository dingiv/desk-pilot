//! 按键定义(键枚举的家)—— KeyEvent / KeyKind / StateFlags。
//!
//! 一次输入始于一枚键;引擎对键的全部决策(消费/透传、提交/选词/翻页)
//! 都以本文件的枚举为词汇,前端只做忠实转换,不做语义解释。
//!
//! 硬编码治理(round10):键语义曾以裸字符(`' '`/`'\n'`/`'\x08'`)在
//! 管线里传递与比较 —— stage1 编码、stage2 解码,两处字面量必须一致才正确。
//! 现在 stage2 的键入口是 [`KeyKind`] 枚举分流(step_key),键名统一
//! [`KeyKind::as_str`],snippet 命令字符映射收拢 [`KeyKind::as_command_char`]。

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
impl KeyKind {
    /// 字符 → KeyKind 归一化(构造语义):控制字符、数字与翻页/移光标符号
    /// 各自成类,其余保持 Char。测试/mock 与前端转换共用此规则。
    pub fn from_char(c: char) -> KeyKind {
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

    /// 键名(日志 / TUI 显示 / 测试断言用)。带载荷的变体只报类名。
    pub fn as_str(&self) -> &'static str {
        match self {
            KeyKind::Char(_) => "Char",
            KeyKind::Digit(_) => "Digit",
            KeyKind::Space => "Space",
            KeyKind::Enter => "Enter",
            KeyKind::Backspace => "Backspace",
            KeyKind::Escape => "Escape",
            KeyKind::Tab => "Tab",
            KeyKind::Up => "Up",
            KeyKind::Down => "Down",
            KeyKind::Left => "Left",
            KeyKind::Right => "Right",
            KeyKind::PageUp => "PageUp",
            KeyKind::PageDown => "PageDown",
            KeyKind::Home => "Home",
            KeyKind::End => "End",
            KeyKind::Delete => "Delete",
            KeyKind::Insert => "Insert",
            KeyKind::BracketLeft => "BracketLeft",
            KeyKind::BracketRight => "BracketRight",
            KeyKind::Plus => "Plus",
            KeyKind::Equal => "Equal",
            KeyKind::Minus => "Minus",
            KeyKind::Function(_) => "Function",
            KeyKind::Modifier => "Modifier",
            KeyKind::Other(_) => "Other",
        }
    }

    /// Snippet 命令态的键 → 命令文本字符(`?num=2` 的数字、`#req` URL 的
    /// `-` `[` `]`…)。仅命令组合态适用 —— 由 stage1 在该态 hoist。
    pub fn as_command_char(&self) -> Option<char> {
        match self {
            KeyKind::Digit(n) => Some(char::from(b'0' + n)),
            KeyKind::Plus => Some('+'),
            KeyKind::Equal => Some('='),
            KeyKind::Minus => Some('-'),
            KeyKind::BracketLeft => Some('['),
            KeyKind::BracketRight => Some(']'),
            _ => None,
        }
    }
}

impl KeyEvent {
    /// 从一个字符构造(测试 / mock / 旧 predict 路径)。控制字符与 1-9
    /// 自动归一到对应 KeyKind —— 与前端转换规则一致。
    pub fn char(c: char) -> Self {
        KeyEvent {
            kind: KeyKind::from_char(c),
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
            kind: KeyKind::from_char(c),
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
        0xffb0 => KeyKind::from_char('0'),
        d @ 0xffb1..=0xffb9 => KeyKind::Digit((d - 0xffb0) as u8),
        _ => match char::from_u32(unicode) {
            Some(c) if !c.is_control() => KeyKind::from_char(c),
            _ => KeyKind::Other(sym),
        },
    }
}

// ── StateFlags:状态标志位 ───────────────────────────────────────────────

/// 状态机表的标志位 —— 一个 bit 表示"当前处于某种输入状态"。
/// 每次按键路由后从 [`StateMachine`] 重新同步(见 [`StateMachine`])。
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


#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn key_names_cover_all_variants() {
        // as_str 是键的唯一显示词汇 —— 每个变体都有名,不带载荷泄漏。
        let all = [
            KeyKind::Char('a'),
            KeyKind::Digit(1),
            KeyKind::Space,
            KeyKind::Enter,
            KeyKind::Backspace,
            KeyKind::Escape,
            KeyKind::Tab,
            KeyKind::Up,
            KeyKind::Down,
            KeyKind::Left,
            KeyKind::Right,
            KeyKind::PageUp,
            KeyKind::PageDown,
            KeyKind::Home,
            KeyKind::End,
            KeyKind::Delete,
            KeyKind::Insert,
            KeyKind::BracketLeft,
            KeyKind::BracketRight,
            KeyKind::Plus,
            KeyKind::Equal,
            KeyKind::Minus,
            KeyKind::Function(1),
            KeyKind::Modifier,
            KeyKind::Other(0),
        ];
        for k in all {
            assert!(!k.as_str().is_empty(), "{k:?} 缺键名");
            assert!(!k.as_str().contains('('), "{k:?} 键名不应带载荷");
        }
    }

    #[test]
    fn from_char_normalizes_control_and_nav_keys() {
        // 字符构造与前端转换同规则:控制/导航字符归一到键变体。
        assert_eq!(KeyKind::from_char(' '), KeyKind::Space);
        assert_eq!(KeyKind::from_char('\n'), KeyKind::Enter);
        assert_eq!(KeyKind::from_char('\r'), KeyKind::Enter);
        assert_eq!(KeyKind::from_char('\x08'), KeyKind::Backspace);
        assert_eq!(KeyKind::from_char('\x7f'), KeyKind::Backspace);
        assert_eq!(KeyKind::from_char('\x1b'), KeyKind::Escape);
        assert_eq!(KeyKind::from_char('\t'), KeyKind::Tab);
        assert_eq!(KeyKind::from_char('5'), KeyKind::Digit(5));
        assert_eq!(KeyKind::from_char('a'), KeyKind::Char('a'));
        assert_eq!(KeyKind::from_char('0'), KeyKind::Char('0')); // 终止符 quirk
    }

    #[test]
    fn command_chars_come_from_keys() {
        // snippet 命令文本的字符一律经 as_command_char 从键导出,
        // 管线里不再出现裸的 `?num=2` 符号字面量。
        assert_eq!(KeyKind::Digit(3).as_command_char(), Some('3'));
        assert_eq!(KeyKind::Plus.as_command_char(), Some('+'));
        assert_eq!(KeyKind::Equal.as_command_char(), Some('='));
        assert_eq!(KeyKind::Minus.as_command_char(), Some('-'));
        assert_eq!(KeyKind::BracketLeft.as_command_char(), Some('['));
        assert_eq!(KeyKind::BracketRight.as_command_char(), Some(']'));
        assert_eq!(KeyKind::Space.as_command_char(), None);
        assert_eq!(KeyKind::Char('x').as_command_char(), None);
    }

    #[test]
    fn fcitx_keysym_roundtrip() {
        // keysym → 键:功能键/小键盘/修饰键归一(值见 keysymdef.h)。
        assert_eq!(KeyEvent::from_fcitx(0xff0d, 0, false, false, false).kind, KeyKind::Enter);
        assert_eq!(KeyEvent::from_fcitx(0xff08, 0, false, false, false).kind, KeyKind::Backspace);
        assert_eq!(KeyEvent::from_fcitx(0xffb5, 0, false, false, false).kind, KeyKind::Digit(5));
        assert_eq!(KeyEvent::from_fcitx(0xffe1, 0, false, false, false).kind, KeyKind::Modifier);
        assert_eq!(KeyEvent::from_fcitx(0xffbe, 0, false, false, false).kind, KeyKind::Function(1));
        // ASCII 可打印区:sym 无映射时用 unicode。
        assert_eq!(KeyEvent::from_fcitx(0x61, 'a' as u32, false, false, false).kind, KeyKind::Char('a'));
    }
}
