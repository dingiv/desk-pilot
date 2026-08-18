//! MagicMember — one magic command (a Member of the Magic family).
//!
//! `#asr`, `#req`, `#date` … are all members of [`MagicFamily`]. A **live** member
//! owns an interactive session: after its trigger completes, the FSM enters
//! [`ComposeState::Magic`] and routes keys + async ticks to the spawned member
//! instance. A **static** member never activates — it resolves to a fixed
//! expansion text inline.
//!
//! ## Adding a command
//! Implement [`MagicMember`] and register it in [`MagicFamily::new`] — matcher
//! entries, prediction hints and activation dispatch are all generated from the
//! registry. No engine / FSM special-casing needed.

use crate::platform::ImeView;
use crate::state::{StateMachine, StepEnv};

/// Result of feeding one key to the active live member.
#[derive(Debug)]
pub enum MemberAction {
    /// The member consumed the key — show this view and stay active.
    /// Boxed: [`ImeView`] is ~3.5 KB(repr(C) 候选槽数组),裸存会把每个
    /// 瞬态 MemberAction(包括 24 字节的 Commit/Exit)都撑到同尺寸。
    View(Box<ImeView>),
    /// The member is done — commit this text and return to Idle.
    Commit(String),
    /// The member is done — exit without committing (cancel).
    Exit,
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
                if pair.is_empty() { continue; }
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
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// 参数字符集:`/path` 与 `?query` 允许的字符(字母/数字 + 常用 URL 保留
/// 标点)。数字在参数开启后也算参数字符(`?num=2` 的 `2`)。
pub fn is_arg_char(c: char) -> bool {
    c == '/' || c == '?' || c.is_ascii_alphanumeric() || "=&_-.%~".contains(c)
}

/// A single magic command.
pub trait MagicMember: Send + Sync {
    /// Command name, also the trigger suffix (e.g. "asr" → "#asr").
    fn name(&self) -> &'static str;

    /// Short description shown in the prediction hint.
    fn description(&self) -> &'static str;

    /// Matcher activation token (e.g. "__ASR_BUFFER__"). When the user completes
    /// the trigger, the FSM looks this token up in the registry and spawns a
    /// fresh instance. `None` = not a live command.
    fn activation_token(&self) -> Option<&'static str> {
        None
    }

    /// Extra triggers that resolve to this same member (e.g. "#flush" → voice).
    fn aliases(&self) -> &[&'static str] {
        &[]
    }

    /// Fresh per-context instance. Each activation gets its own — a member holds
    /// per-session state (typed suffix, last-seen version, …); shared resources
    /// live behind `Arc`s.
    fn spawn(&self) -> Box<dyn MagicMember>;

    /// Enter the command: build the initial candidates / preedit into `sm`.
    fn activate(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> ImeView;

    /// One key while this member is active.
    fn on_key(&mut self, sm: &mut StateMachine, ch: char, env: &dyn StepEnv) -> MemberAction;

    /// Async refresh — called by the engine's tick loop (TUI render / fcitx5
    /// timer). Return `Some(view)` if the member rebuilt the candidate view.
    fn tick(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> Option<ImeView>;

    /// The member session ended (commit / cancel / reset). In-flight background
    /// work keeps running via shared `Arc`s — nothing to cancel by default.
    fn deactivate(&mut self) {}

    /// Full (un-truncated) texts of the current candidates, for display paths
    /// that want the commit-able text (TUI detailed view). Default: the previews
    /// currently in `sm.candidates`.
    fn candidate_texts(&self, sm: &StateMachine) -> Vec<String> {
        sm.candidates.clone()
    }
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
        assert_eq!(a.query, vec![("num".to_string(), "2".to_string()), ("lang".to_string(), "en".to_string())]);
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

    #[test]
    fn arg_char_set() {
        for c in ['/', '?', 'a', 'Z', '0', '=', '&', '_', '-', '.', '%', '~'] {
            assert!(is_arg_char(c), "{c:?} should be an arg char");
        }
        assert!(!is_arg_char(' '));
        assert!(!is_arg_char('#')); // '#' is a trigger, not an arg char
    }
}
