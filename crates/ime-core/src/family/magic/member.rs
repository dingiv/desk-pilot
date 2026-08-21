//! MagicMember — one magic command (a Member of the Magic family).
//!
//! `#asr`, `#req`, `#date`, `#/hello` … are all members of [`MagicFamily`]. In the
//! **prediction model**, a member is a prediction provider: when the user input
//! exactly matches a command (with optional args), the FSM asks it for prediction
//! options via [`MagicMember::predict`]; the options appear in the candidate list
//! (front), the raw trigger is the last rollback. Selecting a prediction commits
//! it to screen unless it's **interactive** — interactive options are handed back
//! to the member via [`MagicMember::pick`], which advances the interaction and the
//! FSM re-queries `predict` (e.g. `#req` fires the request, then shows the body).
//!
//! ## Adding a command
//! Implement [`MagicMember`] and register it in [`MagicFamily::new`] — matcher
//! entries, prediction hints and activation dispatch are all generated from the
//! registry. No engine / FSM special-casing needed.

use crate::state::{StateMachine, StepEnv};

/// 命令的一条预测选项。
///
/// 一个选项携带**三种文本**,各自独立:
/// - [`text`](Prediction::text):候选行里展示的文本;
/// - [`preedit`](Prediction::preedit):应用文本框里的预览(`None` = 用 `text`);
/// - [`commit_text`](Prediction::commit_text):实际提交的文本(`None` = 用 `text`)。
///
/// 典型场景(addon 候选):候选行展示精简结果,preedit 给完整预览,提交原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prediction {
    /// 候选行里展示的文本。可能做转义(`#clip` 把换行显示成 `\n`)。
    pub text: String,
    /// 应用文本框里的预览(preedit);`None` = 与展示文本相同。
    pub preedit: Option<String>,
    /// 提交文本;None = 与展示文本相同。展示转义/截断时,提交用原始文本。
    pub commit_text: Option<String>,

    /// 上屏时光标落点(字节偏移,`$CURSOR` 片段用);None = 末尾。
    pub cursor: Option<usize>,

    /// 交互式:选中不上屏,结果传给命令重新预测(替换选项);
    /// false = 选中即上屏。
    pub interactive: bool,
}

impl Prediction {
    pub fn commit(text: impl Into<String>) -> Self {
        Prediction {
            text: text.into(),
            preedit: None,
            interactive: false,
            cursor: None,
            commit_text: None,
        }
    }

    /// 展示文本与提交文本不同(如 `#clip` 换行转义展示、原文提交)。
    pub fn commit_raw(display: impl Into<String>, raw: impl Into<String>) -> Self {
        Prediction {
            text: display.into(),
            preedit: None,
            interactive: false,
            cursor: None,
            commit_text: Some(raw.into()),
        }
    }

    /// 三文本完全区分:候选展示 `display`,preedit 预览 `preedit`,提交 `raw`。
    pub fn commit_triple(
        display: impl Into<String>,
        preedit: impl Into<String>,
        raw: impl Into<String>,
    ) -> Self {
        Prediction {
            text: display.into(),
            preedit: Some(preedit.into()),
            interactive: false,
            cursor: None,
            commit_text: Some(raw.into()),
        }
    }

    pub fn interactive(text: impl Into<String>) -> Self {
        Prediction {
            text: text.into(),
            preedit: None,
            interactive: true,
            cursor: None,
            commit_text: None,
        }
    }

    pub fn with_cursor(text: impl Into<String>, cursor: usize) -> Self {
        Prediction {
            text: text.into(),
            preedit: None,
            interactive: false,
            cursor: Some(cursor),
            commit_text: None,
        }
    }

    /// 实际提交文本(展示 ≠ 提交时用 commit_text)。
    pub fn commit_value(&self) -> &str {
        self.commit_text.as_deref().unwrap_or(&self.text)
    }

    /// preedit 预览文本(未单独指定时用展示文本)。
    pub fn preedit_value(&self) -> &str {
        self.preedit.as_deref().unwrap_or(&self.text)
    }
}

// ── Command arguments:路径参数(/path)与查询参数(?query)───────────────

/// 魔法命令的路径/查询参数,从命令后的原始字符串解析:
///
/// ```text
/// #asr/en        → path=["en"],            query=[]
/// #asr/en/more   → path=["en","more"],     query=[]
/// #asr?num=2     → path=[],                query=[("num","2")]
/// #asr/en?num=2  → path=["en"],            query=[("num","2")]
/// ```
///
/// 由 [`parse`](CommandArgs::parse) 解析后**传递给家族内部**(member)——
/// member 据此路由(`/en` 翻译、`?num=2` 提交最近 N 条)。这是路径的
/// "留白":解析与分发已就绪,具体语义由各 member 决定(可空实现)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandArgs {
    /// `/` 分隔的路径段(空段忽略)。
    pub path: Vec<String>,
    /// `?` 后 `&` 分隔、`=` 拆分的查询键值对(无 `=` 时值为空串)。
    pub query: Vec<(String, String)>,
}

impl CommandArgs {
    /// 解析原始参数串(`/en?num=2`、`?num=2`、`/unknown` …)。
    pub fn parse(raw: &str) -> CommandArgs {
        let mut args = CommandArgs::default();
        let (path_part, query_part) = match raw.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (raw, None),
        };
        for seg in path_part.split('/') {
            if !seg.is_empty() {
                args.path.push(seg.to_string());
            }
        }
        if let Some(q) = query_part {
            for pair in q.split('&') {
                if pair.is_empty() {
                    continue;
                }
                match pair.split_once('=') {
                    Some((k, v)) => args.query.push((k.to_string(), v.to_string())),
                    None => args.query.push((pair.to_string(), String::new())),
                }
            }
        }
        args
    }

    /// 是否携带某路径段(`#asr/en` → `has_path("en")`)。
    pub fn has_path(&self, seg: &str) -> bool {
        self.path.iter().any(|p| p == seg)
    }

    /// 查询参数取值(`?num=2` → `get("num") == Some("2")`)。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// A single magic command — a prediction provider.
pub trait MagicMember: Send + Sync {
    /// Command name, also the trigger suffix (e.g. "asr" → "#asr"). Empty for the
    /// snippet command (`#/…`).
    fn name(&self) -> &'static str;

    /// Matcher activation token (e.g. "__ASR_BUFFER__"). The FSM spawns a fresh
    /// instance on exact match. `None` = not a live command.
    fn activation_token(&self) -> Option<&'static str> {
        None
    }

    /// Extra triggers that resolve to this same member (e.g. "#flush" → voice).
    fn aliases(&self) -> &[&'static str] {
        &[]
    }

    /// Fresh per-context instance. Each activation gets its own — a member holds
    /// per-session state (req's async status, …); shared resources live behind
    /// `Arc`s.
    fn spawn(&self) -> Box<dyn MagicMember>;

    /// 精确匹配(含参数)时的预测选项(不含 rollback)。`ctx` 是所属输入上下文
    /// (成员可发异步事件 + 订阅);`input` 是完整输入(如 `#asr?num=2`)。
    /// 返回空 = 无预测(只剩 rollback)。
    fn predict(&mut self, ctx: usize, input: &str, env: &dyn StepEnv) -> Vec<Prediction>;

    /// 用户选中了第 `index` 个**交互式**预测。成员更新内部状态后,调用方
    /// 重新查询 `predict` 替换选项(不上屏)。非交互预测不经过这里。
    fn pick(&mut self, index: usize, text: &str, sm: &mut StateMachine, env: &dyn StepEnv) {
        let _ = (index, text, sm, env); // 默认:无交互副作用
    }

    /// 异步刷新预测(live 成员:voice 版本 / req 结果落地)。返回 `Some(新预测)`
    /// 表示候选变了,调用方据此更新;`None` = 没变。
    fn tick(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> Option<Vec<Prediction>> {
        let _ = (sm, env);
        None
    }

    /// The member session ended (commit / cancel / reset). 传 ctx 供退订
    /// (如 VoiceMember 取消对 I/O 线程 watcher 的订阅)。In-flight background
    /// work keeps running via shared `Arc`s — nothing to cancel by default.
    fn deactivate(&mut self, _ctx: usize) {}
}

// ── Shared display helpers ───────────────────────────────────────────────

/// Max displayed bytes for a live candidate preview (≈20 CJK chars). Longer texts get
/// `"first…"` — the full text lives in the member's own state and is committed by Space,
/// so truncation here is cosmetic.
pub const CANDIDATE_PREVIEW_MAX: usize = 60;

/// First `max` bytes (char-boundary-safe, never splits a multi-byte char) + `…` if
/// truncated.
pub fn preview_text(text: &str, max: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_path_only() {
        let a = CommandArgs::parse("/en");
        assert_eq!(a.path, vec!["en".to_string()]);
        assert!(a.query.is_empty());
        assert!(a.has_path("en"));
    }

    #[test]
    fn parse_query_only() {
        let a = CommandArgs::parse("?num=2");
        assert!(a.path.is_empty());
        assert_eq!(a.get("num"), Some("2"));
    }

    #[test]
    fn parse_path_and_query_together() {
        let a = CommandArgs::parse("/en/more?num=2&lang=en");
        assert_eq!(a.path, vec!["en".to_string(), "more".to_string()]);
        assert_eq!(
            a.query,
            vec![
                ("num".to_string(), "2".to_string()),
                ("lang".to_string(), "en".to_string())
            ]
        );
        assert_eq!(a.get("num"), Some("2"));
        assert_eq!(a.get("lang"), Some("en"));
        assert_eq!(a.get("missing"), None);
    }

    #[test]
    fn parse_empty_and_flags() {
        let a = CommandArgs::parse("");
        assert!(a.path.is_empty() && a.query.is_empty());
        // 无值的查询参数(如 ?flag)值为空串。
        let b = CommandArgs::parse("?flag");
        assert_eq!(b.get("flag"), Some(""));
    }
}
