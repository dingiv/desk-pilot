//! ReqMember — 本地 magic HTTP 后端的**通用请求成员**:既承载内置 `#req`,
//! 也支持配置化 addon 插件命令。
//!
//! ## 命令路径 → URL 映射
//!
//! 每个 addon 在配置里声明若干**完整命令路径**(`eg`、`eg/name`、`eg1`),每条
//! 独立参与匹配与预测;`?` 后的查询参数模板(`eg/name?nick=1&len=10`)存于成员,
//! 供执行时构造请求,不参与预测。
//!
//! - 内置 `#req`: `#req<path?query>` → `POST {req_base}<path?query>`(不拼命令名)。
//! - addon `#eg/name?nick=5` → `POST {addon.url}/eg/name?nick=5`(匹配到的完整路径)。
//!
//! ## 触发
//!
//! 完整路径精确匹配即**自动发请求**(无需回车);后缀变化重新请求,version 门控
//! (参照 `#req` 的 `last_version`)。参数输入态(`#eg/1` 这类非注册路径)由框架
//! 展示裸输入提交候选,提交时才经 `predict` 触发。
//!
//! ## 响应
//!
//! 服务器返回 JSON 候选列表;非 JSON 的纯文本回退为单个可提交候选:
//! ```json
//! { "candidates": [ { "text": "候选文本", "interactive": false, "commit_value": "提交文本" } ] }
//! ```
//! `interactive: true` 的候选被选中时,把该文本作为 `pick=<urlencoded>` 参数
//! 拼回 URL 重新请求,服务器据此继续预测。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::member::{MagicMember, Prediction};
use super::MagicResources;
use super::FamilyEnv;

/// Default local magic backend — override via [`MagicFamily::set_req_base`] (or the
/// frontend's config: `magic.req_base`).
pub const DEFAULT_REQ_BASE: &str = "http://127.0.0.1:14555/api";

/// 配置化 addon 插件:一条 `magic.addons` 项。
#[derive(Debug, Clone, Default)]
pub struct AddonConfig {
    /// addon 标识(日志/诊断用)。
    pub name: String,
    /// addon 服务地址(`http://127.0.0.1:9788`)。
    pub url: String,
    /// 注册的命令**路径模板**列表。每条形如 `eg/name?nick=1&len=10`:
    /// 路径部分(`eg/name`)参与匹配/预测;`?` 后的查询参数模板(键=默认值,
    /// 空默认 = 必填)参与执行时的请求构造,不参与预测。
    /// `eg?param1=&param2=2` → 路径 `eg`,param1 必填、param2 默认 "2"。
    pub cmds: Vec<String>,
}

/// 一条 addon 命令路径模板的查询参数。
#[derive(Debug, Clone)]
pub struct AddonParam {
    pub key: String,
    /// 默认值;`None` = 必填(配置里写成 `key=` 空值)。
    pub default: Option<String>,
}

/// 解析后的 addon 命令路径模板。
#[derive(Debug, Clone)]
pub struct AddonCmdSpec {
    /// 完整命令路径(不含 `#`,如 `eg/name`)。
    pub path: String,
    /// 该路径的查询参数模板。
    pub params: Vec<AddonParam>,
}

impl AddonCmdSpec {
    /// 解析 `eg/name?nick=1&len=10` → 路径 `eg/name`,参数 nick(默认 "1")/len(默认 "10")。
    pub fn parse(s: &str) -> AddonCmdSpec {
        let (path, q) = match s.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (s, None),
        };
        let mut params = Vec::new();
        if let Some(q) = q {
            for pair in q.split('&') {
                if pair.is_empty() {
                    continue;
                }
                let (k, v) = match pair.split_once('=') {
                    Some((k, v)) => (k, v),
                    None => (pair, ""),
                };
                params.push(AddonParam {
                    key: k.to_string(),
                    default: if v.is_empty() { None } else { Some(v.to_string()) },
                });
            }
        }
        AddonCmdSpec {
            path: path.to_string(),
            params,
        }
    }
}

/// HTTP POST provider, injected into the family. The production impl
/// (reqwest async, behind the `http` cargo feature) runs on the IoThread
/// event loop — 魔法命令的异步 IO 正门(round10:REQ 与 ASR 共用同一
/// 异步框架,零阻塞线程);tests inject a fake. POST 的 JSON body 携带
/// 结构化参数(`cmd`/`path`/`query`/`pick`),便于扩展更多参数。
///
/// 返回 `BoxFuture`(dyn 兼容 —— fetcher 以 `Arc<dyn ReqFetcher>` 注入)。
pub trait ReqFetcher: Send + Sync {
    /// POST `url` with a JSON `body`; return the response body (plain text) or an
    /// error message.
    fn post(&self, url: &str, body: &str) -> futures::future::BoxFuture<'_, Result<String, String>>;
}

/// Always-failing fetcher — active when ime-core is built without the `http`
/// feature; the frontend must inject a real one via [`crate::engine::ImeEngine::set_req_fetcher`].
#[cfg(not(feature = "http"))]
pub struct NoopFetcher;

#[cfg(not(feature = "http"))]
impl ReqFetcher for NoopFetcher {
    fn post(&self, _url: &str, _body: &str) -> futures::future::BoxFuture<'_, Result<String, String>> {
        Box::pin(async { Err("HTTP 未启用（ime-core 需开启 http feature）".into()) })
    }
}

/// reqwest-backed fetcher (feature `http`). One async client, shared —
/// 请求跑在 IoThread 事件循环上,不占阻塞线程。
#[cfg(feature = "http")]
pub struct ReqwestFetcher {
    client: reqwest::Client,
}

#[cfg(feature = "http")]
impl ReqwestFetcher {
    pub fn new(timeout: std::time::Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        ReqwestFetcher { client }
    }
}

#[cfg(feature = "http")]
impl ReqFetcher for ReqwestFetcher {
    fn post(&self, url: &str, body: &str) -> futures::future::BoxFuture<'_, Result<String, String>> {
        let fut = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send();
        Box::pin(async move {
            let resp = fut.await.map_err(|e| e.to_string())?;
            let status = resp.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            resp.text().await.map_err(|e| e.to_string())
        })
    }
}

/// 响应 JSON:`{ "candidates": [{ "text", "interactive", "commit_value" }] }`。
#[derive(serde::Deserialize)]
struct AddonResponse {
    #[serde(default)]
    candidates: Vec<AddonCandidate>,
}

#[derive(serde::Deserialize)]
struct AddonCandidate {
    /// 候选行展示文本。
    #[serde(default)]
    text: String,
    /// 应用文本框的 preedit 预览(可选;缺省用 `text`)。
    #[serde(default)]
    preedit: Option<String>,
    #[serde(default)]
    interactive: bool,
    /// 实际提交文本(可选;缺省用 `text`)。
    #[serde(default)]
    commit_value: Option<String>,
}

/// 把服务器响应体解析为预测候选列表。JSON 候选优先;非 JSON 纯文本回退为
/// 单个可提交候选(兼容旧 `#req` 后端返回 body 文本)。
fn parse_response(body: &str) -> Vec<Prediction> {
    if let Ok(resp) = serde_json::from_str::<AddonResponse>(body) {
        let preds: Vec<Prediction> = resp
            .candidates
            .into_iter()
            .map(|c| {
                if c.interactive {
                    Prediction::interactive(c.text)
                } else {
                    let commit = c.commit_value.unwrap_or_else(|| c.text.clone());
                    match c.preedit {
                        Some(p) => Prediction::commit_triple(c.text, p, commit),
                        None => Prediction::commit_raw(c.text, commit),
                    }
                }
            })
            .collect();
        if let Some(h) = preds.first() {
            tracing::debug!(
                text = %h.text,
                preedit = %h.preedit_value(),
                commit = %h.commit_value(),
                "req parsed (text/preedit/commit)"
            );
        }
        return preds;
    }
    let t = body.trim();
    if t.is_empty() {
        Vec::new()
    } else {
        vec![Prediction::commit(t.to_string())]
    }
}

/// 把后缀(`/name?nickname=1`)拆成 (path, query 对象)。
fn split_suffix(suffix: &str) -> (String, serde_json::Value) {
    let (path, q) = match suffix.split_once('?') {
        Some((p, q)) => (p.to_string(), q),
        None => (suffix.to_string(), ""),
    };
    let mut query = serde_json::Map::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        query.insert(k.to_string(), serde_json::Value::String(v.to_string()));
    }
    (path, serde_json::Value::Object(query))
}

/// Async status of one request session. The worker thread writes it; `tick` reads
/// it; `version` gates view rebuilds.
#[derive(Debug, Default)]
enum ReqStatus {
    /// Not fired yet.
    #[default]
    Idle,
    /// Worker thread in flight.
    InFlight,
    /// Response candidates.
    Done(Vec<Prediction>),
    /// Fetch failed — error message shown as a non-committable candidate.
    Failed(String),
}

#[derive(Default)]
struct ReqAsync {
    status: Mutex<ReqStatus>,
    version: AtomicU64,
}

pub struct ReqMember {
    resources: Arc<MagicResources>,
    /// 主命令名(`#eg` 的 eg)。addon 场景泄漏为 `&'static str`(注册一次,
    /// spawn 共享同一指针)。
    name: &'static str,
    /// 唯一激活 token(addon 按 addon名+cmd 生成)。
    token: &'static str,
    /// addon 服务地址;`None` = 内置 `#req`,用共享 `resources.req_base`。
    base_url: Option<String>,
    /// 是否把命令名拼进 URL 路径(addon=true;内置 #req=false)。
    prepend_cmd: bool,
    /// addon 注册的命令**路径模板**(`eg`、`eg/name`、`eg1`…)。空 = 内置 `#req`。
    specs: Vec<AddonCmdSpec>,
    /// 本次输入匹配到的注册路径(如 `eg/name`)。URL 用它作命令路径。
    cur_path: Option<String>,
    /// 当前后缀(匹配路径之后的部分,path+query)。`None` = 尚未发过请求。
    arg: Option<String>,
    /// 链式预测的上游文本(`X'#translate` 的 X 求值结果)。`$upstream`
    /// 占位符的替换源;变化时结果失效重发。
    upstream: Option<String>,
    /// Last `version` seen — `tick` compares to detect the worker landing.
    last_version: u64,
    async_state: Arc<ReqAsync>,
}

/// 泄漏一个 String 为 `&'static str`(addon 命令名/别名/token 注册一次、进程内
/// 常驻,泄漏量级为每条命令几个字符串,可接受)。
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// 极简 percent-encode(查询参数值):非保留字符原样,其余 `%XX`。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 后缀里 `$upstream` 占位符 → 上游文本(percent-encoded;无上游 → 空串)。
fn substitute_upstream(s: &str, upstream: Option<&str>) -> String {
    if !s.contains("$upstream") {
        return s.to_string();
    }
    let up = urlencode(upstream.unwrap_or(""));
    s.replace("$upstream", &up)
}

impl ReqMember {
    /// 内置 `#req`:URL 不拼命令名,base 用共享 `req_base`。
    pub fn new_req(resources: Arc<MagicResources>) -> Self {
        ReqMember {
            resources,
            name: "req",
            token: "__REQ__",
            base_url: None,
            prepend_cmd: false,
            specs: Vec::new(),
            cur_path: None,
            arg: None,
            upstream: None,
            last_version: 0,
            async_state: Arc::new(ReqAsync::default()),
        }
    }

    /// addon 命令成员:`#<path><suffix>` → `{base_url}/{path}<suffix>`。
    /// 一个 addon 注册**多条完整路径**(`eg`、`eg/name`、`eg1`),每条独立参与
    /// 精确匹配与前缀预测;`name` 为主路径(如 `eg`),token 按 addon 名区分。
    pub fn new_addon(
        resources: Arc<MagicResources>,
        addon_name: String,
        name: String,
        specs: Vec<AddonCmdSpec>,
        base_url: String,
    ) -> Self {
        let token = leak(format!("__ADDON_{addon_name}_{name}__"));
        ReqMember {
            resources,
            name: leak(name),
            token,
            base_url: Some(base_url),
            prepend_cmd: true,
            specs,
            cur_path: None,
            arg: None,
            upstream: None,
            last_version: 0,
            async_state: Arc::new(ReqAsync::default()),
        }
    }

    /// 输入里**匹配到的注册路径**之后的后缀(path+query,如 `/name?nickname=1`)。
    /// 内置 `#req` 无注册路径,退化为 strip `#req`。
    fn args_of(&mut self, input: &str) -> String {
        let base = format!("#{}", self.name);
        if !self.prepend_cmd || self.specs.is_empty() {
            self.cur_path = None;
            return input.strip_prefix(&base).unwrap_or("").to_string();
        }
        // 找最长匹配的注册路径(如 `#eg/name`),剩余部分作后缀。
        let mut best: Option<&str> = None;
        for spec in &self.specs {
            let t = format!("#{}", spec.path);
            if input.starts_with(&t)
                && best.map(|b| spec.path.len() > b.len()).unwrap_or(true)
            {
                best = Some(&spec.path);
            }
        }
        if let Some(p) = best {
            self.cur_path = Some(p.to_string());
            input.strip_prefix(&format!("#{p}")).unwrap_or("").to_string()
        } else {
            self.cur_path = None;
            input.strip_prefix(&base).unwrap_or("").to_string()
        }
    }

    /// 本次请求的完整 URL。addon 用匹配到的完整路径(`eg/name`)+ 后缀。
    fn url(&self) -> String {
        let base = self
            .base_url
            .clone()
            .unwrap_or_else(|| self.resources.req_base.lock().unwrap().clone());
        // 用户未输后缀时,用配置模板的默认参数(如 `translate?text=$upstream`
        // 的 text)构造 —— 上下文占位符($upstream)由此落地。
        let suffix = match self.arg.as_deref() {
            Some("") | None => self.default_suffix(),
            Some(s) => s.to_string(),
        };
        let suffix = substitute_upstream(&suffix, self.upstream.as_deref());
        if self.prepend_cmd {
            let path = self.cur_path.as_deref().unwrap_or(self.name);
            format!("{base}/{path}{suffix}")
        } else {
            format!("{base}{suffix}")
        }
    }

    /// 当前匹配路径的模板默认后缀(`?key=default&…`);无模板参数则空串。
    fn default_suffix(&self) -> String {
        let Some(spec) = self
            .specs
            .iter()
            .find(|s| Some(&s.path) == self.cur_path.as_ref())
            .or(self.specs.first())
        else {
            return String::new();
        };
        if spec.params.is_empty() {
            return String::new();
        }
        let q: Vec<String> = spec
            .params
            .iter()
            .map(|p| {
                let v = p
                    .default
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("");
                format!("{}={}", p.key, urlencode(v))
            })
            .collect();
        format!("?{}", q.join("&"))
    }

    /// 预测:未发/请求中 → 交互占位;done → 服务器候选;fail → 不可提交错误。
    fn predictions(&mut self) -> Vec<Prediction> {
        let status = &*self.async_state.status.lock().unwrap();
        match status {
            ReqStatus::Idle | ReqStatus::InFlight => {
                vec![Prediction::interactive("请求中…")]
            }
            ReqStatus::Done(preds) => {
                if let Some(h) = preds.first() {
                    tracing::debug!(
                        text = %h.text,
                        preedit = %h.preedit_value(),
                        commit = %h.commit_value(),
                        "req predictions (text/preedit/commit)"
                    );
                }
                preds.clone()
            }
            ReqStatus::Failed(err) => vec![Prediction::interactive(format!("请求失败: {err}"))],
        }
    }

    /// 输入后缀变了 → 之前的 Done 结果不再适用,复位为 Idle。
    fn invalidate_result(&self) {
        if let Ok(mut st) = self.async_state.status.lock() {
            if matches!(&*st, ReqStatus::Done(_)) {
                *st = ReqStatus::Idle;
            }
        }
    }

    /// 结构化请求体:`{cmd, path, query, pick?}`。POST 时带上,方便扩展参数。
    /// `cmd` 用匹配到的完整路径(`eg/name`),后端据此知道命中了哪条命令路径。
    fn request_body(&self, pick: Option<&str>) -> String {
        let suffix = self.arg.as_deref().unwrap_or("");
        let (path, query) = split_suffix(suffix);
        let cmd = self.cur_path.as_deref().unwrap_or(self.name);
        let mut obj = serde_json::json!({
            "cmd": cmd,
            "path": path,
            "query": query,
        });
        if let Some(p) = pick {
            obj["pick"] = serde_json::Value::String(p.to_string());
        }
        // 链式上游原文(POST body 双保险:URL 占位符之外,服务端也可从
        // body 读,避免 URL 长度/转义问题)。
        if let Some(u) = &self.upstream {
            obj["upstream"] = serde_json::Value::String(u.clone());
        }
        obj.to_string()
    }

    /// 触发 POST:任务发到引擎 I/O 线程(阻塞池执行,不卡事件循环)。结果落
    /// `async_state` + 版本号,I/O 线程随后 `refresh_ui(ctx)` 推送重渲染。
    fn fire(&self, ctx: usize) {
        let url = self.url();
        let body = self.request_body(None);
        self.fire_url(ctx, url, body);
    }

    /// 用指定 URL + body 触发(interactive 候选重发带 `pick=` 时用)。
    fn fire_url(&self, ctx: usize, url: String, body: String) {
        let fetcher = self.resources.req_fetcher.lock().unwrap().clone();
        let shared = Arc::clone(&self.async_state);
        *shared.status.lock().unwrap() = ReqStatus::InFlight;
        shared.version.fetch_add(1, Ordering::Release);
        tracing::debug!(url, ?body, "req fire");
        match self.resources.io() {
            // 异步正门:请求跑在 IoThread 事件循环上(真异步,零阻塞线程),
            // 完成后由 IoThread 统一 refresh_ui(ctx)。
            Some(io) => io.spawn_task(ctx, async move {
                let result = fetcher.post(&url, &body).await;
                let st = match result {
                    Ok(body) => {
                        let preds = parse_response(&body);
                        tracing::debug!(url, count = preds.len(), "req response parsed");
                        ReqStatus::Done(preds)
                    }
                    Err(e) => ReqStatus::Failed(e),
                };
                tracing::debug!(url, ok = matches!(st, ReqStatus::Done(_)), "req done");
                *shared.status.lock().unwrap() = st;
                shared.version.fetch_add(1, Ordering::Release);
            }),
            // 无 I/O 线程(未接线的测试场景)→ 轻量 executor 就地执行
            // (假 fetcher 返回 ready future;生产永远有 IoThread)。
            None => {
                let result = futures::executor::block_on(fetcher.post(&url, &body));
                let st = match result {
                    Ok(body) => ReqStatus::Done(parse_response(&body)),
                    Err(e) => ReqStatus::Failed(e),
                };
                *shared.status.lock().unwrap() = st;
                shared.version.fetch_add(1, Ordering::Release);
            }
        }
    }
}

impl MagicMember for ReqMember {
    fn name(&self) -> &'static str {
        self.name
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some(self.token)
    }

    /// addon 注册的全部命令完整路径(`eg`、`eg/name`、`eg1`…);内置 `#req`
    /// 只有命令名自身。
    fn registered_paths(&self) -> Vec<String> {
        if self.specs.is_empty() {
            vec![self.name.to_string()]
        } else {
            self.specs.iter().map(|s| s.path.clone()).collect()
        }
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(ReqMember {
            resources: Arc::clone(&self.resources),
            name: self.name,
            token: self.token,
            base_url: self.base_url.clone(),
            prepend_cmd: self.prepend_cmd,
            specs: self.specs.clone(),
            cur_path: None,
            arg: None,
            upstream: None,
            last_version: 0,
            async_state: Arc::new(ReqAsync::default()),
        })
    }

    fn predict(&mut self, ctx: usize, input: &str, _env: &dyn FamilyEnv) -> Vec<Prediction> {
        // 后缀从输入派生;匹配路径/后缀任一变化(含首次)→ 自动发请求。
        let prev_path = self.cur_path.clone();
        let arg = self.args_of(input);
        if self.arg.as_deref() != Some(&arg) || prev_path != self.cur_path {
            self.arg = Some(arg);
            self.invalidate_result();
            self.fire(ctx);
        }
        // 消费当前版本 —— tick 只对之后的异步落地触发重建。
        self.last_version = self.async_state.version.load(Ordering::Acquire);
        self.predictions()
    }

    /// 链式上下文声明:任一模板参数默认值含 `$upstream`(如
    /// `translate?text=$upstream`)→ 感知上游(First)。声明即配置,无需代码。
    fn wants_context(&self) -> Option<super::member::ContextKind> {
        let wants = self.specs.iter().any(|s| {
            s.params
                .iter()
                .any(|p| p.default.as_deref().is_some_and(|v| v.contains("$upstream")))
        });
        wants.then_some(super::member::ContextKind::First)
    }

    /// 带上游的预测:记录上游文本,上游变化时结果失效并强制重发(arg 未变
    /// 也要重发 —— predict 的变化检测只看命令后缀)。
    fn predict_with_context(
        &mut self,
        ctx: usize,
        input: &str,
        upstream: &super::member::ChainContext,
        env: &dyn FamilyEnv,
    ) -> Vec<Prediction> {
        let up = upstream.first_text().to_string();
        if self.upstream.as_deref() != Some(up.as_str()) {
            self.upstream = Some(up);
            self.invalidate_result();
            self.arg = None; // 强制 predict 走"后缀变化"重发路径
        }
        self.predict(ctx, input, env)
    }

    fn pick(&mut self, _index: usize, text: &str, ctx: usize, _env: &dyn FamilyEnv) {
        // 只有服务器返回的交互候选才把文本传回重发;请求中/失败等状态占位不重发。
        let is_done = matches!(&*self.async_state.status.lock().unwrap(), ReqStatus::Done(_));
        if is_done {
            let url = self.url();
            let body = self.request_body(Some(text));
            self.fire_url(ctx, url, body);
        }
    }

    fn tick(&mut self, ctx: usize, buffer: &str, env: &dyn FamilyEnv) -> Option<Vec<Prediction>> {
        let cur = self.async_state.version.load(Ordering::Acquire);
        if cur == self.last_version {
            return None; // no request finished since the last rebuild
        }
        self.last_version = cur;
        Some(self.predict(ctx, buffer, env))
    }
}
