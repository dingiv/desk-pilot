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
mod concat;
mod del;
mod member;
pub mod voice_state;
pub use voice_state::SharedVoiceState;
pub mod expander;
mod req;
mod snippet;
mod translate;
mod voice;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub use clip::{ClipMember, CLIP_HISTORY_CAP};
pub use concat::ConcatMember;
pub use del::DelMember;
pub use member::{
    preview_text, ChainContext, CommandArgs, ContextKind, MagicMember, Prediction,
    CANDIDATE_PREVIEW_MAX,
};
pub use req::{AddonCmdSpec, AddonConfig, ReqFetcher, DEFAULT_REQ_BASE};
pub use snippet::SnippetMember;
pub use crate::family::FamilyEnv;
pub use translate::TranslateMember;
pub use voice::VoiceMember;

use req::ReqMember;

use crate::store::snippet_md::SnippetEntry;

/// SharedVoiceState 槽 —— voice listener task 在 IoThread 上折叠 SSE 段写入,
/// 魔法成员 (`#asr` / `#submit`) 同步读。引擎构造时一次性注入。
#[derive(Default)]
pub struct VoiceStateSlot(Mutex<Option<Arc<voice_state::SharedVoiceState>>>);

impl VoiceStateSlot {
    pub fn set(&self, state: Arc<voice_state::SharedVoiceState>) {
        *self.0.lock().unwrap() = Some(state);
    }

    pub fn get(&self) -> Option<Arc<voice_state::SharedVoiceState>> {
        self.0.lock().unwrap().clone()
    }
}

/// Resources shared between the engine and all member instances (across input
/// contexts): the shared voice state and the `#req` backend config. Members grab
/// `Arc` clones at spawn, so late attachment (start-up ordering) is fine.
pub struct MagicResources {
    pub voice_state: Arc<VoiceStateSlot>,
    pub req_base: Mutex<String>,
    pub req_fetcher: Mutex<Arc<dyn ReqFetcher>>,
    /// 片段注册表:名字(无前导 `/`)→ 片段条目(模板 + 候选区注释 + 声明参数)。
    /// `#/hello?name=Mike` 的 `hello` 在此查表;`?name=Mike` 作为模板变量注入。
    pub snippets: Mutex<HashMap<String, SnippetEntry>>,
    /// 剪贴板历史(最近在前,`#clip/N` 读)。由前端按需回填(fcitx5 clipboard
    /// 公开接口只给当前值)。
    pub clipboard_history: Mutex<Vec<String>>,
    /// 引擎的单条 tokio I/O 线程 —— 魔法命令发事件让它做异步 I/O。
    /// 引擎构造后经 [`MagicFamily::set_io`] 注入;此前为 None。
    pub io: Mutex<Option<Arc<crate::io_thread::IoThread>>>,
    /// 前端句柄 —— I/O 线程经它推送 UI 刷新 / 请求剪贴板。
    pub frontend: Mutex<Option<Arc<dyn crate::frontend::FrontEndHandle>>>,
    /// voice server 命令 sender —— `#asr` 家族经它发 `Attach`/`Detach`。
    /// 与 io thread 的 `tx` 是同一个通道(typed 包装)。引擎构造后注入。
    pub voice_tx: Mutex<Option<crate::io_thread::VoiceCmdSender>>,
    /// 最近一次输入法提交文本的 UTF-8 字符数(`#del` 的 `del_len` 选项用)。
    pub last_commit_len: Mutex<u32>,
    /// scout(omni-scout)HTTP 注入服务地址 —— `#del` 用它注入退格。
    pub scout_inject_url: Mutex<String>,
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

    /// 取 shared voice state(未注入时 None —— 测试 / 未接线场景)。
    pub fn voice_state(&self) -> Option<Arc<voice_state::SharedVoiceState>> {
        self.voice_state.get()
    }

    /// 取 voice server 命令 sender(未注入时 None)。
    pub fn voice_tx(&self) -> Option<crate::io_thread::VoiceCmdSender> {
        self.voice_tx.lock().unwrap().clone()
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
            voice_state: Arc::new(VoiceStateSlot::default()),
            req_base: Mutex::new(DEFAULT_REQ_BASE.to_string()),
            req_fetcher: Mutex::new(default_fetcher()),
            snippets: Mutex::new(HashMap::new()),
            clipboard_history: Mutex::new(Vec::new()),
            io: Mutex::new(None),
            frontend: Mutex::new(None),
            voice_tx: Mutex::new(None),
            last_commit_len: Mutex::new(0),
            scout_inject_url: Mutex::new("http://127.0.0.1:7878".to_string()),
        }
    }
}

/// Stage2 查询答案(S5):壳直接落位 MagicSession 的三个字段。
#[derive(Debug, Default)]
pub struct MagicAnswer {
    /// 预测选项(不含 rollback;rebuild_magic_view 附加)。
    pub predictions: Vec<Prediction>,
    /// 补全提示(Prefix 态;选中改写输入)。
    pub hints: Vec<String>,
    /// 数字键是否用于选中候选(false = 数字是命令/参数文本)。
    pub selectable: bool,
}

/// 魔法命令解析结果的数据对:activation token(spawn 实例用)+ 命令名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveCommand {
    pub token: &'static str,
    pub name: &'static str,
}

/// 输入匹配结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MagicMatch {
    /// 精确匹配某命令(完整路径,可带查询参数,如 `#asr?num=2`、`#eg/name?nick=5`)。
    Exact(LiveCommand),
    /// 输入是某注册完整路径的严格前缀 → 补全提示(选中改写输入)。
    Prefix(Vec<String>),
    /// **参数输入态**:输入是某注册命令路径 + `/` 或 `?` 参数(如 `#del/15`)。
    /// 框架展示裸输入提交候选,不自动触发;提交时解析前缀并强制触发。
    Args(LiveCommand),
    /// 片段命令(`#/…`)精确匹配。
    Snippet,
    /// 无匹配。
    Unknown,
}

// `today_str` lives in the expander (shared by `$DATE` variables + `#date`).

pub struct MagicFamily {
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
            Arc::new(DelMember::new(Arc::clone(&resources))),
            Arc::new(ReqMember::new_req(Arc::clone(&resources))),
            Arc::new(ClipMember::new(Arc::clone(&resources))),
            Arc::new(ConcatMember::new()),
            Arc::new(TranslateMember::new()),
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
            members,
            token_map,
            resources,
        }
    }

    /// All matcher entries: static triggers → their trigger itself as a SENTINEL;
    /// live triggers → the activation token (per registered full path).
    ///
    /// Statics are NOT frozen into the matcher: `expansion()` is time-varying
    /// (`#date`), and calling it here would commit the ENGINE-STARTUP date when
    /// the trigger completes days later. The FSM detects the sentinel
    /// (expansion == trigger) and resolves the static FRESH from the registry.
    /// A user snippet shadowing the same trigger has expansion ≠ trigger, so
    /// the override is untouched.
    /// idle 键是否触发器引导符 —— **只有 `#`**(命令 `#asr`;片段 `#/name`
    /// 同样以 `#` 进入,`/` 是命令文本的一部分)。单独的 `/` 是普通按键,
    /// 透传给应用 —— 原 matcher trie 的根孩子只有 `#`(空名成员/片段不进
    /// trie,见 matcher_entries),此处必须与其等价,否则 `/` 会被误捕获。
    pub fn is_trigger_start(&self, ch: char) -> bool {
        ch == '#'
    }

    pub fn matcher_entries(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for m in &self.members {
            // 空名成员(片段命令)不进 matcher trie —— 它经 `#` + `/` 特判路由。
            if m.name().is_empty() {
                continue;
            }
            let token = m
                .activation_token()
                .expect("live member needs an activation token");
            for path in m.registered_paths() {
                out.push((format!("#{path}"), token.to_string()));
            }
        }
        out
    }

    /// Spawn a fresh member instance for an activation token (matcher `Complete`).
    /// `None` if the token isn't a live command.
    pub fn spawn(&self, token: &str) -> Option<Box<dyn MagicMember>> {
        let idx = *self.token_map.get(token)?;
        Some(self.members[idx].spawn())
    }

    /// All magic commands whose full trigger is a strict extension of `prefix` — the
    /// completion hints shown while typing `#…`. Selecting a hint rewrites the input
    /// to that trigger. The raw buffer stays a rollback candidate.
    pub fn hints(&self, prefix: &str) -> Vec<String> {
        if prefix.is_empty() || !prefix.starts_with('#') {
            return Vec::new();
        }
        let mut out = Vec::new();
        for m in &self.members {
            if m.name().is_empty() {
                continue;
            } // 片段命令不作为 `#…` 提示
            for path in m.registered_paths() {
                let t = format!("#{path}");
                if t.starts_with(prefix) && t != prefix {
                    out.push(t);
                }
            }
        }
        out
    }

    /// **魔法家族匹配核心逻辑**
    ///
    /// Stage2 统一查询(S5 家族化):非链式 snippet 态的命令预测。
    /// ensure(spawn / 同名保活 / deactivate)与成员 predict 全部内聚;
    /// `active` 是壳持有的 live 实例缓存(保 req 异步态等跨键状态),
    /// 壳只负责把 [`MagicAnswer`] 落位候选面板。`input` 传原始 buffer
    /// (尾部 `'` 链结构字符在此剥除,不参与命令匹配)。
    pub fn query(
        &self,
        active: &mut Option<Box<dyn MagicMember>>,
        ctx: usize,
        input: &str,
        env: &dyn FamilyEnv,
    ) -> MagicAnswer {
        let match_input = input.trim_end_matches('\'').to_string();
        let mut ans = MagicAnswer::default();
        match self.match_command(&match_input) {
            MagicMatch::Exact(LiveCommand { token, name }) => {
                self.ensure_active(active, ctx, name, Some(token));
                ans.predictions = active
                    .as_mut()
                    .map(|m| m.predict(ctx, &match_input, env))
                    .unwrap_or_default();
                // 无参数时数字用于选中;有参数(拼 `?num=` 等)时数字是文本。
                ans.selectable = match_input == format!("#{name}");
            }
            // 参数输入态(`#del/1`):不调 member.predict(不自动触发),展示
            // 裸输入提交候选;提交时 force-fire 解析前缀再触发。
            MagicMatch::Args(LiveCommand { token, name }) => {
                self.ensure_active(active, ctx, name, Some(token));
                ans.predictions = vec![Prediction::submit(match_input)];
            }
            MagicMatch::Prefix(hints) => {
                Self::clear_active(active, ctx);
                ans.hints = hints;
                ans.selectable = true; // 前缀 → 数字选中补全
            }
            MagicMatch::Snippet => {
                self.ensure_active(active, ctx, "", Some("__SNIPPET__"));
                ans.predictions = active
                    .as_mut()
                    .map(|m| m.predict(ctx, &match_input, env))
                    .unwrap_or_default();
                // 片段路径/查询里的数字是文本
            }
            MagicMatch::Unknown => {
                Self::clear_active(active, ctx);
            }
        }
        ans
    }

    /// ensure:同名保活(保 req 异步态),否则 deactivate 旧实例并 spawn 新的。
    fn ensure_active(
        &self,
        active: &mut Option<Box<dyn MagicMember>>,
        ctx: usize,
        name: &'static str,
        token: Option<&'static str>,
    ) {
        let keep = active.as_ref().map(|m| m.name() == name).unwrap_or(false);
        if keep {
            return;
        }
        Self::clear_active(active, ctx);
        if let Some(tok) = token {
            *active = self.spawn(tok);
        }
    }

    /// deactivate 并清空 live 实例。
    fn clear_active(active: &mut Option<Box<dyn MagicMember>>, ctx: usize) {
        if let Some(mut m) = active.take() {
            m.deactivate(ctx);
        }
    }

    /// 按**注册的完整命令路径**(`eg`、`eg/name`、`eg1`…)做匹配:
    /// - **完整路径精确匹配** → `Exact`,立即执行(带 `?` 查询参数也算精确,查询
    ///   不参与匹配/预测);
    /// - **严格前缀** → `Prefix`,给出完整路径补全提示(选中改写输入);
    /// - 输入以某注册路径为前缀、后接 `/` 参数段(如 `#del/1`)→ `Args`,
    ///   参数输入态(裸输入,提交时再解析触发);
    /// - 其余 → `Unknown`。
    ///
    /// 见 [`MagicMatch`]。
    pub fn match_command(&self, input: &str) -> MagicMatch {
        // 片段命令:`#/hello?name=Mike` → 空名命令。
        if input.starts_with("#/") {
            return MagicMatch::Snippet;
        }
        if input.len() < 2 || !input.starts_with('#') {
            return MagicMatch::Unknown;
        }
        // 路径部分:去掉 `?` 查询参数(查询不参与匹配/预测)。
        let path = input.split('?').next().unwrap();
        let rest = &path[1..]; // 去掉 `#`
        if rest.is_empty() {
            return MagicMatch::Unknown;
        }

        // 1. 完整路径精确匹配 → 执行。
        if let Some(cmd) = self.command_for_path(rest) {
            return MagicMatch::Exact(cmd);
        }

        // 2. 前缀匹配:rest 是某注册完整路径的严格前缀 → 补全提示。
        let mut hints = Vec::new();
        for m in &self.members {
            if m.name().is_empty() {
                continue;
            }
            for rp in m.registered_paths() {
                let t = format!("#{rp}");
                if t.starts_with(path) && t != path {
                    hints.push(t);
                }
            }
        }
        if !hints.is_empty() {
            return MagicMatch::Prefix(hints);
        }

        // 3. 参数输入态:rest 以某注册完整路径为前缀,后接 `/` 参数段。
        if let Some(cmd) = self.command_for_arg_prefix(rest) {
            return MagicMatch::Args(cmd);
        }
        MagicMatch::Unknown
    }

    /// 完整路径 → live 命令。
    fn command_for_path(&self, path: &str) -> Option<LiveCommand> {
        for m in &self.members {
            if m.registered_paths().iter().any(|rp| rp == path) {
                if let Some(token) = m.activation_token() {
                    return Some(LiveCommand {
                        token,
                        name: m.name(),
                    });
                }
            }
        }
        None
    }

    /// 参数输入态:找**最长**注册完整路径 `P` 使 `path` 以 `P + "/"` 开头
    /// (即 `path` 是某命令的路径参数扩展,如 `del/1` → `del`)。静态命令无参数。
    fn command_for_arg_prefix(&self, path: &str) -> Option<LiveCommand> {
        let mut best: Option<(usize, LiveCommand)> = None;
        for m in &self.members {
            let Some(token) = m.activation_token() else {
                continue;
            };
            for rp in m.registered_paths() {
                if path.len() > rp.len()
                    && path.starts_with(rp.as_str())
                    && path.as_bytes()[rp.len()] == b'/'
                    && best.as_ref().is_none_or(|(bl, _)| rp.len() > *bl) {
                        best = Some((
                            rp.len(),
                            LiveCommand {
                                token,
                                name: m.name(),
                            },
                        ));
                    }
            }
        }
        best.map(|(_, cmd)| cmd)
    }

    /// Attach the shared voice state — voice listener task 与魔法成员都通过它
    /// 读 / 写。引擎构造时自动调一次(随 `with_config`),外部不需要再调。
    pub fn set_voice_state(&self, state: Arc<voice_state::SharedVoiceState>) {
        self.resources.voice_state.set(state);
    }

    /// `#req` backend base URL (default `http://127.0.0.1:14555/api`).
    pub fn set_req_base(&self, base: &str) {
        *self.resources.req_base.lock().unwrap() = base.to_string();
    }

    /// scout(omni-scout)HTTP 注入服务地址 —— `#del` 用它注入退格。
    pub fn set_scout_inject_url(&self, url: &str) {
        *self.resources.scout_inject_url.lock().unwrap() = url.to_string();
    }

    /// scout 注入服务地址(默认 `http://127.0.0.1:7878`)。
    pub fn scout_inject_url(&self) -> String {
        self.resources.scout_inject_url.lock().unwrap().clone()
    }

    /// Inject an HTTP fetcher (tests use a fake; production default is reqwest
    /// behind the `http` feature).
    pub fn set_req_fetcher(&self, fetcher: Arc<dyn ReqFetcher>) {
        *self.resources.req_fetcher.lock().unwrap() = fetcher;
    }

    /// 填充片段注册表(名字 → 条目)。名字不应带前导 `/`(`#/hello` 的 `hello`)。
    /// 供 SnippetMember 在 `#/name?params` 时查表展开。
    pub fn set_snippets(&self, snippets: Vec<SnippetEntry>) {
        let mut map = self.resources.snippets.lock().unwrap();
        map.clear();
        for s in snippets {
            map.insert(s.name.clone(), s);
        }
    }

    /// 注入 I/O 线程句柄与前端句柄(引擎构造后调用)。魔法命令发事件给 I/O
    /// 线程做异步工作;I/O 完成经前端推送刷新。
    pub fn set_io(
        &self,
        io: Arc<crate::io_thread::IoThread>,
        frontend: Arc<dyn crate::frontend::FrontEndHandle>,
    ) {
        *self.resources.io.lock().unwrap() = Some(io);
        *self.resources.frontend.lock().unwrap() = Some(frontend);
    }

    /// 注入 voice server 命令 sender(`#asr` 家族发 Attach/Detach)。
    pub fn set_voice_tx(&self, tx: crate::io_thread::VoiceCmdSender) {
        *self.resources.voice_tx.lock().unwrap() = Some(tx);
    }

    /// 注册配置化 addon 插件命令(`magic.addons`)。每条 cmds 是**完整路径模板**
    /// (如 `eg/name?nick=1&len=10`):路径部分注册为独立命令路径(`#eg/name`),
    /// `?` 后的参数模板存于成员(执行时构造请求,不参与预测)。**必须在 matcher
    /// 构建前调用**(引擎 `with_config` 里做,此时 magic refcount=1)。
    pub fn register_addons(&mut self, addons: &[AddonConfig]) {
        for a in addons {
            if a.cmds.is_empty() {
                continue;
            }
            let specs: Vec<AddonCmdSpec> = a
                .cmds
                .iter()
                .map(|c| AddonCmdSpec::parse(c))
                .filter(|s| !s.path.is_empty())
                .collect();
            if specs.is_empty() {
                continue;
            }
            let name = specs[0].path.clone();
            let member: Arc<dyn MagicMember> = Arc::new(req::ReqMember::new_addon(
                Arc::clone(&self.resources),
                a.name.clone(),
                name,
                specs,
                a.url.clone(),
            ));
            if let Some(token) = member.activation_token() {
                self.token_map.insert(token, self.members.len());
            }
            self.members.push(member);
        }
    }

    /// 取 voice server 命令 sender(未注入时 None —— 测试 / 未接线场景)。
    pub fn voice_cmd_tx(&self) -> Option<crate::io_thread::VoiceCmdSender> {
        self.resources.voice_tx()
    }

    /// 前端按需回填剪贴板历史(`#clip` 触发 RequestClipboard → 前端取到后
    /// 调这里替换整个历史)。
    pub fn set_clipboard_history(&self, items: Vec<String>) {
        let mut hist = self.resources.clipboard_history.lock().unwrap();
        hist.clear();
        hist.extend(
            items
                .into_iter()
                .filter(|t| !t.is_empty())
                .take(CLIP_HISTORY_CAP),
        );
    }

    /// 推送一条剪贴板文本到历史(最近在前,去重连续重复)。C++ 每次按键/激活
    /// 推送当前剪贴板,`#clip/N` 读这个环。
    pub fn push_clipboard(&self, text: &str) {
        if text.is_empty() {
            return;
        }
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
        // 静态命令已整体移除(原 #date/#password sentinel 不复存在)。
        assert!(
            !entries.iter().any(|(t, _)| t == "#date" || t == "#password"),
            "static commands removed: {entries:?}"
        );
        assert!(entries.contains(&("#asr".into(), "__ASR_BUFFER__".into())));
        assert!(
            !entries.iter().any(|(t, _)| t == "#flush"),
            "#flush alias removed"
        );
        assert!(
            !entries.iter().any(|(t, _)| t == "#submit"),
            "#submit 已删除(遗留命令)"
        );
        assert!(entries.contains(&("#req".into(), "__REQ__".into())));
    }

    #[test]
    fn spawn_resolves_live_tokens_only() {
        let fam = MagicFamily::new();
        assert!(fam.spawn("__ASR_BUFFER__").is_some());
        assert!(fam.spawn("__REQ__").is_some());
        // Static commands and unknown tokens are not live commands.
        assert!(fam.spawn("__NOPE__").is_none());
    }

    #[test]
    fn resources_are_shared_across_clones() {
        // The scorer keeps a clone; the engine keeps the original — both must see
        // the same voice state / req base after a late set_* call.
        let fam = MagicFamily::new();
        let clone = fam.clone();
        let state = Arc::new(voice_state::SharedVoiceState::new());
        fam.set_voice_state(Arc::clone(&state));
        assert!(clone.resources().voice_state.get().is_some(), "voice state shared");
        fam.set_req_base("http://example.test:9/x");
        assert_eq!(
            *clone.resources().req_base.lock().unwrap(),
            "http://example.test:9/x"
        );
    }
}
