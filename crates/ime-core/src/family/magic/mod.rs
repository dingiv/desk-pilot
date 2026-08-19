//! MagicFamily — `#`-prefixed magic commands, unified as a registry of members.
//!
//! Every command is a [`MagicMember`]:
//! - **static** members resolve to a fixed expansion inline (`#date`, `#password`),
//! - **live** members own an interactive session (`#asr` voice anchor, `#req` HTTP
//!   request) — after the trigger completes, the FSM enters `ComposeState::Magic`
//!   and routes keys + async ticks to the spawned member instance.
//!
//! The matcher entries, prediction hints and activation dispatch are ALL generated
//! from this registry — adding a command is one struct + one registration, with no
//! engine / FSM special-casing.

mod clip;
mod member;
mod req;
mod snippet;
mod voice;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub use clip::{ClipMember, CLIP_HISTORY_CAP};
pub use member::{preview_text, CANDIDATE_PREVIEW_MAX, CommandArgs, MagicMember, Prediction};
pub use req::{ReqFetcher, DEFAULT_REQ_BASE};
pub use snippet::SnippetMember;
pub use voice::{SubmitMember, VoiceMember};

use req::ReqMember;

use crate::expander::today_str;

/// Shared voice-session slot — written by the aura SSE client (via
/// [`MagicFamily::set_asr_buffer`], late after engine construction), read by the
/// voice-family member instances. Lives behind an `Arc` so the engine and every
/// per-context member see the same buffer.
#[derive(Default)]
pub struct VoiceSlot(Mutex<Option<Arc<crate::asr_buffer::AsrBuffer>>>);

impl VoiceSlot {
    pub fn set(&self, buf: Arc<crate::asr_buffer::AsrBuffer>) {
        *self.0.lock().unwrap() = Some(buf);
    }

    pub fn get(&self) -> Option<Arc<crate::asr_buffer::AsrBuffer>> {
        self.0.lock().unwrap().clone()
    }
}

/// Resources shared between the engine and all member instances (across input
/// contexts): the voice buffer slot and the `#req` backend config. Members grab
/// `Arc` clones at spawn, so late attachment (start-up ordering) is fine.
pub struct MagicResources {
    pub voice: Arc<VoiceSlot>,
    pub req_base: Mutex<String>,
    pub req_fetcher: Mutex<Arc<dyn ReqFetcher>>,
    /// 片段注册表:名字(无前导 `/`)→ 模板。`#/hello?name=Mike` 的 `hello`
    /// 在此查表;`?name=Mike` 作为模板变量注入。
    pub snippets: Mutex<HashMap<String, String>>,
    /// 剪贴板历史(最近在前,`#clip/N` 读)。由前端按需回填(fcitx5 clipboard
    /// 公开接口只给当前值)。
    pub clipboard_history: Mutex<Vec<String>>,
    /// 引擎的单条 tokio I/O 线程 —— 魔法命令发事件让它做异步 I/O。
    /// 引擎构造后经 [`MagicFamily::set_io`] 注入;此前为 None。
    pub io: Mutex<Option<Arc<crate::io_thread::IoThread>>>,
    /// 前端句柄 —— I/O 线程经它推送 UI 刷新 / 请求剪贴板。
    pub frontend: Mutex<Option<Arc<dyn crate::frontend::FrontEndHandle>>>,
}

impl MagicResources {
    /// 取 I/O 线程句柄(未注入时 None —— 测试/未接线场景)。
    pub fn io(&self) -> Option<Arc<crate::io_thread::IoThread>> {
        self.io.lock().unwrap().clone()
    }

    /// 取前端句柄(未注入时 None)。
    pub fn frontend(&self) -> Option<Arc<dyn crate::frontend::FrontEndHandle>> {
        self.frontend.lock().unwrap().clone()
    }
}

fn default_fetcher() -> Arc<dyn ReqFetcher> {
    #[cfg(feature = "http")]
    {
        Arc::new(req::ReqwestFetcher::new(std::time::Duration::from_secs(5)))
    }
    #[cfg(not(feature = "http"))]
    {
        Arc::new(req::NoopFetcher)
    }
}

impl Default for MagicResources {
    fn default() -> Self {
        MagicResources {
            voice: Arc::new(VoiceSlot::default()),
            req_base: Mutex::new(DEFAULT_REQ_BASE.to_string()),
            req_fetcher: Mutex::new(default_fetcher()),
            snippets: Mutex::new(HashMap::new()),
            clipboard_history: Mutex::new(Vec::new()),
            io: Mutex::new(None),
            frontend: Mutex::new(None),
        }
    }
}

/// 精确匹配到的命令(供状态机 spawn / 展开)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MagicCommand {
    /// 静态命令(无实例,直接展开)。
    Static,
    /// live 命令:token 用于 spawn 实例。
    Live { token: &'static str, name: &'static str },
}

/// 输入匹配结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MagicMatch {
    /// 精确匹配某命令(可带参数,如 `#asr?num=2`)。
    Exact(MagicCommand),
    /// 输入是某命令触发串的严格前缀 → 补全提示(选中改写输入)。
    Prefix(Vec<String>),
    /// 片段命令(`#/…`)精确匹配。
    Snippet,
    /// 无匹配。
    Unknown,
}

/// A static command: fixed expansion text, no interactive session. The expansion
/// is computed on demand (matcher entries freeze it at engine build; prediction
/// hints resolve it fresh).
pub struct StaticCmd {
    pub trigger: &'static str,
    pub description: &'static str,
    expansion: Arc<dyn Fn() -> String + Send + Sync>,
}

impl StaticCmd {
    pub fn new(
        trigger: &'static str,
        description: &'static str,
        expansion: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        StaticCmd { trigger, description, expansion: Arc::new(expansion) }
    }

    pub fn expansion(&self) -> String {
        (self.expansion)()
    }
}

impl Clone for StaticCmd {
    fn clone(&self) -> Self {
        StaticCmd { trigger: self.trigger, description: self.description, expansion: Arc::clone(&self.expansion) }
    }
}

// `today_str` lives in the expander (shared by `$DATE` variables + `#date`).

pub struct MagicFamily {
    /// Static commands (inline expansion).
    statics: Vec<StaticCmd>,
    /// Live commands, each with an activation token.
    members: Vec<Arc<dyn MagicMember>>,
    token_map: HashMap<&'static str, usize>,
    /// Shared resources for member instances (voice slot, req config).
    resources: Arc<MagicResources>,
}

impl MagicFamily {
    pub fn new() -> Self {
        let resources = Arc::new(MagicResources::default());
        let members: Vec<Arc<dyn MagicMember>> = vec![
            Arc::new(VoiceMember::new(Arc::clone(&resources))),
            Arc::new(SubmitMember::new(Arc::clone(&resources))),
            Arc::new(ReqMember::new(Arc::clone(&resources))),
            Arc::new(ClipMember::new(Arc::clone(&resources))),
            // 片段命令:空名魔法命令(`#/hello?name=Mike`),经 `#` + `/` 路由。
            Arc::new(SnippetMember::new(Arc::clone(&resources))),
        ];
        let mut token_map = HashMap::new();
        for (i, m) in members.iter().enumerate() {
            if let Some(tok) = m.activation_token() {
                token_map.insert(tok, i);
            }
        }
        MagicFamily {
            statics: vec![
                StaticCmd::new("#date", "insert today's date", today_str),
                StaticCmd::new("#password", "password manager", || {
                    "[password manager — not yet implemented]".into()
                }),
            ],
            members,
            token_map,
            resources,
        }
    }

    /// All matcher entries: static triggers → their trigger itself as a SENTINEL;
    /// live triggers → the activation token (plus aliases, e.g. `#flush` → voice
    /// token).
    ///
    /// Statics are NOT frozen into the matcher: `expansion()` is time-varying
    /// (`#date`), and calling it here would commit the ENGINE-STARTUP date when
    /// the trigger completes days later. The FSM detects the sentinel
    /// (expansion == trigger) and resolves the static FRESH from the registry.
    /// A user snippet shadowing the same trigger has expansion ≠ trigger, so
    /// the override is untouched.
    pub fn matcher_entries(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for s in &self.statics {
            out.push((s.trigger.to_string(), s.trigger.to_string()));
        }
        for m in &self.members {
            // 空名成员(片段命令)不进 matcher trie —— 它经 `#` + `/` 特判路由。
            if m.name().is_empty() { continue; }
            let token = m.activation_token().expect("live member needs an activation token");
            out.push((format!("#{}", m.name()), token.to_string()));
            for alias in m.aliases() {
                out.push((format!("#{alias}"), token.to_string()));
            }
        }
        out
    }

    /// Spawn a fresh member instance for an activation token (matcher `Complete`).
    /// `None` if the token isn't a live command (static expansion path handles it).
    pub fn spawn(&self, token: &str) -> Option<Box<dyn MagicMember>> {
        let idx = *self.token_map.get(token)?;
        Some(self.members[idx].spawn())
    }

    /// All magic commands whose trigger is a strict extension of `prefix` — the completion
    /// hints shown while typing `#…`. Selecting a hint rewrites the input to that trigger.
    /// The raw buffer stays a rollback candidate.
    pub fn hints(&self, prefix: &str) -> Vec<String> {
        if prefix.is_empty() || !prefix.starts_with('#') {
            return Vec::new();
        }
        let mut out = Vec::new();
        for s in &self.statics {
            if s.trigger.starts_with(prefix) && s.trigger != prefix {
                out.push(s.trigger.to_string());
            }
        }
        for m in &self.members {
            if m.name().is_empty() { continue; } // 片段命令不作为 `#…` 提示
            let t = format!("#{}", m.name());
            if t.starts_with(prefix) && t != prefix {
                out.push(t.clone());
            }
            for alias in m.aliases() {
                let ta = format!("#{alias}");
                if ta.starts_with(prefix) && ta != prefix {
                    out.push(ta.clone());
                }
            }
        }
        out
    }

    /// 输入 → 匹配结果。名字段 = `#` + 字母数字(遇 `/` 或 `?` 截止);找最长
    /// **精确**命令。见 [`MagicMatch`]。
    pub fn match_command(&self, input: &str) -> MagicMatch {
        // 片段命令:`#/hello?name=Mike` → 空名命令。
        if input.starts_with("#/") {
            return MagicMatch::Snippet;
        }
        if input.len() < 2 || !input.starts_with('#') {
            return MagicMatch::Unknown;
        }
        // 名字段:`#` + 字母数字;遇 `/`/`?` 或非字母数字截止。
        let rest = &input[1..];
        let name_len = rest.chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();
        if name_len == 0 {
            return MagicMatch::Unknown;
        }
        let name = &rest[..name_len];
        // 名字段之后必须是 `/`、`?` 或空(否则是别的命令的前缀 / 未知)。
        let name_is_exact = name_len == rest.len()
            || rest[name_len..].starts_with('/')
            || rest[name_len..].starts_with('?');

        if name_is_exact {
            // 精确:先 live 成员(含 alias),后静态。
            for m in &self.members {
                if m.name() == name || m.aliases().contains(&name) {
                    if let Some(token) = m.activation_token() {
                        return MagicMatch::Exact(MagicCommand::Live { token, name: m.name() });
                    }
                }
            }
            if self.statics.iter().any(|s| s.trigger == format!("#{name}")) {
                return MagicMatch::Exact(MagicCommand::Static);
            }
        }

        // 前缀:输入(整个)是某命令触发串的严格前缀。
        let mut hints = Vec::new();
        for s in &self.statics {
            if s.trigger.starts_with(input) && s.trigger != input {
                hints.push(s.trigger.to_string());
            }
        }
        for m in &self.members {
            if m.name().is_empty() { continue; }
            let t = format!("#{}", m.name());
            if t.starts_with(input) && t != input {
                hints.push(t.clone());
            }
            for alias in m.aliases() {
                let ta = format!("#{alias}");
                if ta.starts_with(input) && ta != input {
                    hints.push(ta.clone());
                }
            }
        }
        if !hints.is_empty() {
            return MagicMatch::Prefix(hints);
        }
        MagicMatch::Unknown
    }

    /// Static expansion text for a full trigger (e.g. `#date` → today's date).
    pub fn static_expansion(&self, trigger: &str) -> Option<String> {
        self.statics.iter().find(|s| s.trigger == trigger).map(|s| s.expansion())
    }

    /// 静态命令的预测:展开值作为一条提交预测。
    pub fn static_prediction(&self, trigger: &str) -> Option<Vec<Prediction>> {
        self.statics.iter().find(|s| s.trigger == trigger)
            .map(|s| vec![Prediction::commit(s.expansion())])
    }

    /// Attach the voice buffer — routed to the shared slot all voice members read.
    pub fn set_asr_buffer(&self, buf: Arc<crate::asr_buffer::AsrBuffer>) {
        self.resources.voice.set(buf);
    }

    /// `#req` backend base URL (default `http://127.0.0.1:14555/api`).
    pub fn set_req_base(&self, base: &str) {
        *self.resources.req_base.lock().unwrap() = base.to_string();
    }

    /// Inject an HTTP fetcher (tests use a fake; production default is reqwest
    /// behind the `http` feature).
    pub fn set_req_fetcher(&self, fetcher: Arc<dyn ReqFetcher>) {
        *self.resources.req_fetcher.lock().unwrap() = fetcher;
    }

    /// 填充片段注册表(名字 → 模板)。名字不应带前导 `/`(`#/hello` 的 `hello`)。
    /// 供 SnippetMember 在 `#/name?params` 时查表展开。
    pub fn set_snippets(&self, snippets: Vec<(String, String)>) {
        let mut map = self.resources.snippets.lock().unwrap();
        map.clear();
        for (name, tpl) in snippets {
            map.insert(name, tpl);
        }
    }

    /// 注入 I/O 线程句柄与前端句柄(引擎构造后调用)。魔法命令发事件给 I/O
    /// 线程做异步工作;I/O 完成经前端推送刷新。
    pub fn set_io(&self, io: Arc<crate::io_thread::IoThread>, frontend: Arc<dyn crate::frontend::FrontEndHandle>) {
        *self.resources.io.lock().unwrap() = Some(io);
        *self.resources.frontend.lock().unwrap() = Some(frontend);
    }

    /// 推送一条剪贴板文本到历史(最近在前,去重连续重复)。C++ 每次按键/激活
    /// 推送当前剪贴板,`#clip/N` 读这个环。
    pub fn push_clipboard(&self, text: &str) {
        if text.is_empty() { return; }
        let mut hist = self.resources.clipboard_history.lock().unwrap();
        if hist.first().map(|s| s == text).unwrap_or(false) {
            return; // 连续推送同一项(每次按键都会推)
        }
        hist.insert(0, text.to_string());
        hist.truncate(CLIP_HISTORY_CAP);
    }

    /// Shared resources — member instances and the engine talk through these.
    pub fn resources(&self) -> Arc<MagicResources> {
        Arc::clone(&self.resources)
    }
}

impl Clone for MagicFamily {
    fn clone(&self) -> Self {
        MagicFamily {
            statics: self.statics.clone(),
            members: self.members.clone(),
            token_map: self.token_map.clone(),
            resources: Arc::clone(&self.resources),
        }
    }
}

impl Default for MagicFamily {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_entries_cover_all_commands() {
        let fam = MagicFamily::new();
        let entries: Vec<(String, String)> = fam.matcher_entries();
        // Statics carry a sentinel (trigger == expansion) — resolved FRESH at
        // completion so a long-running engine doesn't commit the startup date.
        assert!(entries.contains(&("#date".into(), "#date".into())), "{entries:?}");
        assert!(entries.contains(&("#password".into(), "#password".into())), "{entries:?}");
        assert!(entries.contains(&("#asr".into(), "__ASR_BUFFER__".into())));
        assert!(!entries.iter().any(|(t, _)| t == "#flush"), "#flush alias removed");
        assert!(entries.contains(&("#submit".into(), "__ASR_SUBMIT__".into())));
        assert!(entries.contains(&("#req".into(), "__REQ__".into())));
    }

    #[test]
    fn spawn_resolves_live_tokens_only() {
        let fam = MagicFamily::new();
        assert!(fam.spawn("__ASR_BUFFER__").is_some());
        assert!(fam.spawn("__REQ__").is_some());
        // Static commands and unknown tokens are not live commands.
        assert!(fam.spawn("__ASR_SUBMIT__").is_some());
        assert!(fam.spawn("__NOPE__").is_none());
    }

    #[test]
    fn resources_are_shared_across_clones() {
        // The scorer keeps a clone; the engine keeps the original — both must see
        // the same req base / voice slot after a late set_* call.
        let fam = MagicFamily::new();
        let clone = fam.clone();
        let buf = Arc::new(crate::asr_buffer::AsrBuffer::new());
        fam.set_asr_buffer(Arc::clone(&buf));
        assert!(clone.resources().voice.get().is_some(), "voice slot shared");
        fam.set_req_base("http://example.test:9/x");
        assert_eq!(*clone.resources().req_base.lock().unwrap(), "http://example.test:9/x");
    }
}
