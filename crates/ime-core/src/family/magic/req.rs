//! ReqMember — `#req`: request the local magic HTTP backend and surface the
//! response as a single candidate.
//!
//! Syntax: `#req` (default endpoint) or `#req<path?query>` — everything typed
//! after the trigger is appended to the configured base URL:
//!
//! ```text
//! #req/news?query=soccer  →  GET http://127.0.0.1:14555/api/news?query=soccer
//! ```
//!
//! Enter / Space fires the request (Space commits the result once it has
//! arrived). The whole response body is ONE candidate — the backend is expected
//! to serve plain text. Esc cancels; Backspace edits the suffix. The fetch runs
//! on a worker thread so the engine never blocks; `tick` picks the result up by
//! version counter, exactly like the voice member picks up aura segments.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::member::{MagicMember, Prediction};
use super::MagicResources;
use crate::state::{StateMachine, StepEnv};

/// Default local magic backend — override via [`MagicFamily::set_req_base`] (or the
/// frontend's config: `magic.req_base`).
pub const DEFAULT_REQ_BASE: &str = "http://127.0.0.1:14555/api";

/// Synchronous HTTP GET provider, injected into the family. The production impl
/// (reqwest, behind the `http` cargo feature) runs on a worker thread; tests
/// inject a fake.
pub trait ReqFetcher: Send + Sync {
    /// GET `url` and return the response body (plain text) or an error message.
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Always-failing fetcher — active when ime-core is built without the `http`
/// feature; the frontend must inject a real one via [`crate::engine::ImeEngine::set_req_fetcher`].
#[cfg(not(feature = "http"))]
pub struct NoopFetcher;

#[cfg(not(feature = "http"))]
impl ReqFetcher for NoopFetcher {
    fn get(&self, _url: &str) -> Result<String, String> {
        Err("HTTP 未启用（ime-core 需开启 http feature）".into())
    }
}

/// reqwest-backed fetcher (feature `http`). One blocking client, shared.
#[cfg(feature = "http")]
pub struct ReqwestFetcher {
    client: reqwest::blocking::Client,
}

#[cfg(feature = "http")]
impl ReqwestFetcher {
    pub fn new(timeout: std::time::Duration) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        ReqwestFetcher { client }
    }
}

#[cfg(feature = "http")]
impl ReqFetcher for ReqwestFetcher {
    fn get(&self, url: &str) -> Result<String, String> {
        let resp = self.client.get(url).send().map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {}", status.as_u16()));
        }
        resp.text().map_err(|e| e.to_string())
    }
}

/// Async status of one `#req` session. The worker thread writes it; `tick` reads
/// it; `version` gates view rebuilds.
#[derive(Debug, Default)]
enum ReqStatus {
    /// Not fired yet — the user is still typing the suffix.
    #[default]
    Idle,
    /// Worker thread in flight.
    InFlight,
    /// Response body — the single committable candidate.
    Done(String),
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
    /// Suffix after the trigger — derived fresh from the input each predict
    /// (path + query, e.g. "/news?query=soccer").
    arg: String,
    /// Last `version` seen — `tick` compares to detect the worker thread landing.
    last_version: u64,
    async_state: Arc<ReqAsync>,
}

impl ReqMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        ReqMember {
            resources,
            arg: String::new(),
            last_version: 0,
            async_state: Arc::new(ReqAsync::default()),
        }
    }

    /// The URL this member would request: configured base + typed suffix.
    fn url(&self) -> String {
        let base = self.resources.req_base.lock().unwrap().clone();
        format!("{base}{}", self.arg)
    }

    /// 预测:一个选项 —— 未发:交互式"回车请求 <url>"(选中触发);in-flight:
    /// 交互式"请求中…";done:提交完整 body(展示截断由前端做);fail:不可提交错误。
    fn predictions(&mut self) -> Vec<Prediction> {
        let status = &*self.async_state.status.lock().unwrap();
        match status {
            ReqStatus::Idle => vec![Prediction::interactive(format!("回车请求 {}", self.url()))],
            ReqStatus::InFlight => vec![Prediction::interactive(String::from("请求中…"))],
            ReqStatus::Done(body) => vec![Prediction::commit(body.clone())],
            ReqStatus::Failed(err) => vec![Prediction::interactive(format!("请求失败: {err}"))],
        }
    }

    /// 输入的 URL 后缀变了 → 之前的 Done 结果不再适用,复位为 Idle。
    fn invalidate_result(&self) {
        if let Ok(mut st) = self.async_state.status.lock() {
            if matches!(&*st, ReqStatus::Done(_)) {
                *st = ReqStatus::Idle;
            }
        }
    }

    /// 触发 GET:把任务发给引擎的 I/O 线程(事件响应模型,预测主路径不建
    /// 线程)。任务在 I/O 线程跑,结果落 `async_state` + 版本号,I/O 线程随后
    /// `refresh_ui(ctx)` 推送前端重渲染。
    fn fire(&self, ctx: usize) {
        let url = self.url();
        let fetcher = self.resources.req_fetcher.lock().unwrap().clone();
        let shared = Arc::clone(&self.async_state);
        *shared.status.lock().unwrap() = ReqStatus::InFlight;
        shared.version.fetch_add(1, Ordering::Release);
        tracing::debug!(url, "req fire");
        match self.resources.io() {
            Some(io) => io.send(crate::io_thread::IoEvent::Run {
                ctx,
                task: Box::new(move || {
                    let result = fetcher.get(&url);
                    let st = match result {
                        Ok(body) => ReqStatus::Done(body),
                        Err(e) => ReqStatus::Failed(e),
                    };
                    tracing::debug!(url, ok = matches!(st, ReqStatus::Done(_)), "req done");
                    *shared.status.lock().unwrap() = st;
                    shared.version.fetch_add(1, Ordering::Release);
                }),
            }),
            // 无 I/O 线程(未接线的测试场景)→ 就地执行。
            None => {
                let result = fetcher.get(&url);
                let st = match result {
                    Ok(body) => ReqStatus::Done(body),
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
        "req"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__REQ__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(ReqMember::new(Arc::clone(&self.resources)))
    }

    fn predict(&mut self, _ctx: usize, input: &str, _env: &dyn StepEnv) -> Vec<Prediction> {
        // 后缀从输入派生;变了 → 旧结果失效。
        let arg = input.strip_prefix("#req").unwrap_or("").to_string();
        if arg != self.arg {
            self.arg = arg;
            self.invalidate_result();
        }
        // 消费当前版本 —— tick 只对之后的异步落地触发重建。
        self.last_version = self.async_state.version.load(Ordering::Acquire);
        self.predictions()
    }

    fn pick(&mut self, _index: usize, _text: &str, sm: &mut StateMachine, _env: &dyn StepEnv) {
        // 交互式选项:Idle 的"回车请求 <url>" → 触发;InFlight/Failed 无副作用。
        let idle = matches!(&*self.async_state.status.lock().unwrap(), ReqStatus::Idle);
        if idle {
            self.fire(sm.ctx);
        }
    }

    fn tick(&mut self, sm: &mut StateMachine, env: &dyn StepEnv) -> Option<Vec<Prediction>> {
        let cur = self.async_state.version.load(Ordering::Acquire);
        if cur == self.last_version {
            return None; // no request finished since the last rebuild
        }
        self.last_version = cur;
        let input = sm.buffer.clone();
        Some(self.predict(sm.ctx, &input, env))
    }
}

// ── Real-HTTP e2e (feature `http` only) ─────────────────────────────────

#[cfg(all(test, feature = "http"))]
mod http_tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;

    /// One-shot HTTP server on an ephemeral port: serves one response, then closes.
    fn serve_once(body: &'static str, status: u16) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_owned();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf); // consume the request
            let reason = if status == 200 { "OK" } else { "Not Found" };
            let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body.as_bytes());
        });
        (format!("http://{addr}/"), handle)
    }

    #[test]
    fn reqwest_fetcher_gets_utf8_body() {
        let (base, server) = serve_once("你好, reqwest!", 200);
        let fetcher = ReqwestFetcher::new(Duration::from_secs(5));
        let out = fetcher
            .get(&format!("{base}api/news?query=soccer"))
            .unwrap();
        assert_eq!(out, "你好, reqwest!");
        server.join().unwrap();
    }

    #[test]
    fn reqwest_fetcher_surfaces_http_error() {
        let (base, server) = serve_once("nope", 404);
        let fetcher = ReqwestFetcher::new(Duration::from_secs(5));
        let err = fetcher.get(&base).unwrap_err();
        assert!(err.contains("404"), "{err}");
        server.join().unwrap();
    }
}
