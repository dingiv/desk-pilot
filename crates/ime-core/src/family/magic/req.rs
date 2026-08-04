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

use super::member::{preview_text, CANDIDATE_PREVIEW_MAX, MagicMember, MemberAction};
use super::MagicResources;
use crate::platform::ImeView;
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
#[derive(Debug)]
enum ReqStatus {
    /// Not fired yet — the user is still typing the suffix.
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

impl Default for ReqStatus {
    fn default() -> Self {
        ReqStatus::Idle
    }
}

pub struct ReqMember {
    resources: Arc<MagicResources>,
    /// Suffix typed after the trigger: path + query (e.g. "/news?query=soccer").
    arg: String,
    /// Committable text of the latest result (the body) while status is `Done`.
    full: Option<String>,
    /// Last `version` seen — `tick` compares to detect the worker thread landing.
    last_version: u64,
    async_state: Arc<ReqAsync>,
}

impl ReqMember {
    pub fn new(resources: Arc<MagicResources>) -> Self {
        ReqMember {
            resources,
            arg: String::new(),
            full: None,
            last_version: 0,
            async_state: Arc::new(ReqAsync::default()),
        }
    }

    /// The URL this member would request: configured base + typed suffix.
    fn url(&self) -> String {
        let base = self.resources.req_base.lock().unwrap().clone();
        format!("{base}{}", self.arg)
    }

    /// Rebuild the candidate view from the async status. One candidate:
    /// the hint (Idle), a status line (InFlight/Failed), or the body preview (Done).
    fn rebuild(&mut self, sm: &mut StateMachine) -> ImeView {
        let status = &*self.async_state.status.lock().unwrap();
        let (candidate, full): (String, Option<String>) = match status {
            ReqStatus::Idle => (format!("回车请求 {}", self.url()), None),
            ReqStatus::InFlight => ("请求中…".into(), None),
            ReqStatus::Done(body) => (preview_text(body, CANDIDATE_PREVIEW_MAX), Some(body.clone())),
            ReqStatus::Failed(err) => (format!("请求失败: {err}"), None),
        };
        self.full = full;
        self.last_version = self.async_state.version.load(Ordering::Acquire);
        sm.candidates = vec![candidate];
        sm.candidates_fresh = true;
        sm.candidate_highlight = 0;
        sm.candidate_page = 0;
        sm.preedit = format!("#req{}", self.arg);
        sm.cursor = sm.preedit.len();
        sm.make_view()
    }

    /// A typed suffix invalidates a previous result — the URL changed.
    fn invalidate_result(&self) {
        if let Ok(mut st) = self.async_state.status.lock() {
            if matches!(&*st, ReqStatus::Done(_)) {
                *st = ReqStatus::Idle;
            }
        }
    }

    /// Fire the GET on a worker thread; the result lands in `async_state` and
    /// bumps the version. The thread holds its own `Arc`, so canceling the
    /// member mid-flight is safe — the result is simply dropped.
    fn fire(&self) {
        let url = self.url();
        let fetcher = self.resources.req_fetcher.lock().unwrap().clone();
        let shared = Arc::clone(&self.async_state);
        *shared.status.lock().unwrap() = ReqStatus::InFlight;
        shared.version.fetch_add(1, Ordering::Release);
        tracing::debug!(url, "req fire");
        std::thread::spawn(move || {
            let result = fetcher.get(&url);
            let st = match result {
                Ok(body) => ReqStatus::Done(body),
                Err(e) => ReqStatus::Failed(e),
            };
            tracing::debug!(url, ok = matches!(st, ReqStatus::Done(_)), "req done");
            *shared.status.lock().unwrap() = st;
            shared.version.fetch_add(1, Ordering::Release);
        });
    }
}

impl MagicMember for ReqMember {
    fn name(&self) -> &'static str {
        "req"
    }

    fn description(&self) -> &'static str {
        "request local magic backend"
    }

    fn activation_token(&self) -> Option<&'static str> {
        Some("__REQ__")
    }

    fn spawn(&self) -> Box<dyn MagicMember> {
        Box::new(ReqMember::new(Arc::clone(&self.resources)))
    }

    fn activate(&mut self, sm: &mut StateMachine, _env: &dyn StepEnv) -> ImeView {
        self.rebuild(sm)
    }

    fn on_key(&mut self, sm: &mut StateMachine, ch: char, _env: &dyn StepEnv) -> MemberAction {
        match ch {
            '\x08' => {
                if self.arg.pop().is_some() {
                    self.invalidate_result();
                    MemberAction::View(self.rebuild(sm))
                } else {
                    // Suffix already empty — Backspace cancels the session.
                    MemberAction::Exit
                }
            }
            '\x1b' => MemberAction::Exit,
            ' ' | '\n' | '\r' => {
                let done = match &*self.async_state.status.lock().unwrap() {
                    ReqStatus::Done(body) => Some(body.clone()),
                    _ => None,
                };
                match done {
                    // Result present → Space/Enter commits it.
                    Some(body) => MemberAction::Commit(body),
                    // Idle/Failed → fire the request; InFlight → ignore (view refresh).
                    None => {
                        self.fire();
                        MemberAction::View(self.rebuild(sm))
                    }
                }
            }
            d @ '1'..='9' => {
                let has_result =
                    matches!(&*self.async_state.status.lock().unwrap(), ReqStatus::Done(_));
                if has_result {
                    // Only one candidate (the whole body) — digit 1 selects it.
                    match (d == '1').then(|| self.full.clone()).flatten() {
                        Some(t) => MemberAction::Commit(t),
                        None => MemberAction::View(self.rebuild(sm)),
                    }
                } else {
                    // No result yet — digits are URL characters, extend the suffix.
                    self.arg.push(d);
                    MemberAction::View(self.rebuild(sm))
                }
            }
            // URL-ish characters extend the suffix (path + query).
            c if c.is_ascii_alphanumeric() || "/?&=:.%+-_~".contains(c) => {
                self.arg.push(c);
                self.invalidate_result();
                MemberAction::View(self.rebuild(sm))
            }
            _ => MemberAction::View(StateMachine::passthrough_view()),
        }
    }

    fn tick(&mut self, sm: &mut StateMachine, _env: &dyn StepEnv) -> Option<ImeView> {
        let cur = self.async_state.version.load(Ordering::Acquire);
        if cur == self.last_version {
            return None; // no request finished since the last rebuild
        }
        tracing::debug!(last_version = self.last_version, cur, "req tick rebuild");
        Some(self.rebuild(sm))
    }

    fn candidate_texts(&self, sm: &StateMachine) -> Vec<String> {
        match &self.full {
            Some(body) => vec![body.clone()],
            None => sm.candidates.clone(),
        }
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
        let out = fetcher.get(&format!("{base}api/news?query=soccer")).unwrap();
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
