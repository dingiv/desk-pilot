//! OpenAI 兼容的 remote provider 实现 (`reqwest::blocking`, 同步)。
//!
//! 适配 vLLM / SGLang / qwen3-asr-rs `asr-server` / 任意 OpenAI 兼容服务。
//!
//! 同时暴露 [`HttpLlm::warm`] / [`HttpLlm::warm_with_options`]:针对 **dp-router**
//! 控制面的预热接口——确保模型已 online 可调用。供上层(aura-daemon 启动、UI 切模型)
//! 在首次 inference 前主动探活 + 触发动态加载,避免首次请求的 latency spike。

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::{AsrProvider, LlmProvider, ModelProvider, VlmProvider};

/// ASR via OpenAI `/v1/audio/transcriptions` (multipart wav)。
pub struct HttpAsr {
    client: reqwest::blocking::Client,
    endpoint: String,
    /// 服务端模型名(必传;OpenAI 规范要求 multipart form 里带 `model` 字段)。
    /// 需与目标服务的模型注册名对齐(如 dp-router.yaml `models[].name`)。
    model: String,
}

impl HttpAsr {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::blocking::Client::new(),
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }
}

impl ModelProvider for HttpAsr {
    fn kind(&self) -> &'static str {
        "remote-http"
    }
}

impl AsrProvider for HttpAsr {
    fn recognize(&self, pcm: &[i16], sr: u32) -> Result<String> {
        let wav = pcm_to_wav(pcm, sr);
        let part = reqwest::blocking::multipart::Part::bytes(wav)
            .file_name("audio.wav")
            .mime_str("audio/wav")?;
        let form = reqwest::blocking::multipart::Form::new()
            .text("model", self.model.clone())
            .part("file", part);
        let resp: TranscriptionResp = self
            .client
            .post(url(&self.endpoint, "/v1/audio/transcriptions"))
            .multipart(form)
            .send()?
            .error_for_status()?
            .json()?;
        Ok(resp.text)
    }
}

/// LLM via OpenAI `/v1/chat/completions`。
pub struct HttpLlm {
    client: reqwest::blocking::Client,
    endpoint: String,
    model: String,
}

impl HttpLlm {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::blocking::Client::new(),
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }

    /// 探测/触发 dp-router 加载模型,直到 `self.model` 在线可调用。
    ///
    /// 调用语义:
    /// - 模型已 online → 立即返 `Ok`(`already_online=true`)
    /// - 模型未加载或 offline/starting → `POST /admin/models/load` 触发,然后 poll 直到 online
    /// - 模型在 dp-router models_root 中找不到 → `Err(WarmError::ModelNotFound)`
    /// - 超过 `load_timeout_s` 仍 online → `Err(WarmError::LoadTimeout)`
    /// - dp-router 服务端标 `offline`(spawn 失败/重启预算耗尽)→ `Err(WarmError::RouterError)`
    ///
    /// 典型调用方:
    /// - **aura-daemon 启动**:`LlmSpec::Remote` 解析后,后台 spawn 一个 thread 调 `warm()`
    ///   一次,提前消除首次 Stage2 的 latency spike;主流程不阻塞
    /// - **UI 切模型**:用户在 UI 上选了一个之前没加载的模型,前端调 warm() 显示进度
    ///
    /// **注意**:`warm()` 只对 dp-router 端点有效。若 `self.endpoint` 是普通 OpenAI
    /// 兼容服务(没有 `/admin/models`),返 [`WarmError::NotDpRouter`],不会 panic。
    pub fn warm(&self) -> Result<WarmOutcome, WarmError> {
        self.warm_with_options(WarmOptions::default())
    }

    /// 同 [`warm`](Self::warm),但带可调参数。
    pub fn warm_with_options(&self, opts: WarmOptions) -> Result<WarmOutcome, WarmError> {
        let base = derive_base_url(&self.endpoint);
        let start = Instant::now();

        // 1. 初次 GET /admin/models — 快速路径
        let snap = self.fetch_snapshot(&base)?;
        if let Some(m) = snap.models.iter().find(|m| m.name == self.model) {
            if m.status == AdminStatus::Online {
                return Ok(WarmOutcome {
                    already_online: true,
                    load_triggered: false,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }
            // offline 或 starting → 继续等(可能是健康检查失败正在重启)
        }

        // 2. POST /admin/models/load — 触发动态加载(若 404,直接 ModelNotFound)
        self.trigger_load(&base)?;

        // 3. Poll /admin/models 直到 online / timeout / 服务端标 offline
        let deadline = Instant::now() + Duration::from_secs(opts.load_timeout_s);
        let poll = Duration::from_millis(opts.poll_interval_ms);
        loop {
            std::thread::sleep(poll);
            if Instant::now() >= deadline {
                return Err(WarmError::LoadTimeout(opts.load_timeout_s));
            }
            let snap = match self.fetch_snapshot(&base) {
                Ok(s) => s,
                Err(e) => return Err(e), // 网络断开之类
            };
            match snap.models.iter().find(|m| m.name == self.model) {
                Some(m) => match m.status {
                    AdminStatus::Online => {
                        return Ok(WarmOutcome {
                            already_online: false,
                            load_triggered: true,
                            elapsed_ms: start.elapsed().as_millis() as u64,
                        });
                    }
                    AdminStatus::Offline => {
                        return Err(WarmError::LoadFailed {
                            model: self.model.clone(),
                            reason: "dp-router marked model offline (spawn failed or restart budget exhausted)".into(),
                        });
                    }
                    AdminStatus::Starting | AdminStatus::Unknown => continue,
                },
                None => continue, // POST 已 202 但还没注册到表 — 等
            }
        }
    }

    fn fetch_snapshot(&self, base: &str) -> Result<AdminSnapshot, WarmError> {
        let url = format!("{}/admin/models", base.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .send()
            .map_err(|e| WarmError::RouterUnreachable(url.clone(), e))?;
        let status = resp.status();
        if !status.is_success() {
            // 端点不是 dp-router(没有 /admin/models) → 404 → 明确错误
            if status.as_u16() == 404 {
                return Err(WarmError::NotDpRouter(self.endpoint.clone()));
            }
            let body = resp.text().unwrap_or_default();
            return Err(WarmError::RouterError {
                path: url,
                status: status.as_u16(),
                body,
            });
        }
        let body = resp
            .text()
            .map_err(|e| WarmError::RouterUnreachable(url.clone(), e))?;
        serde_json::from_str::<AdminSnapshot>(&body).map_err(WarmError::Parse)
    }

    fn trigger_load(&self, base: &str) -> Result<(), WarmError> {
        let url = format!("{}/admin/models/load", base.trim_end_matches('/'));
        let body = serde_json::json!({ "name": self.model });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .map_err(|e| WarmError::RouterUnreachable(url.clone(), e))?;
        let status = resp.status();
        if status.is_success() {
            // 200(已在线,幂等)或 202(开始加载) — 都视为成功触发
            return Ok(());
        }
        if status.as_u16() == 404 {
            return Err(WarmError::ModelNotFound(self.model.clone()));
        }
        let body = resp.text().unwrap_or_default();
        Err(WarmError::RouterError {
            path: url,
            status: status.as_u16(),
            body,
        })
    }
}

impl ModelProvider for HttpLlm {
    fn kind(&self) -> &'static str {
        "remote-http"
    }
}

impl LlmProvider for HttpLlm {
    fn complete(&self, system: &str, user: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let resp: ChatResp = self
            .client
            .post(url(&self.endpoint, "/v1/chat/completions"))
            .json(&body)
            .send()?
            .error_for_status()?
            .json()?;
        Ok(resp.choices.into_iter().next().ok_or_else(|| anyhow!("no choices in response"))?.message.content)
    }
}

/// VLM via OpenAI `/v1/chat/completions` (image as `data:image/png;base64,...` URL)。
pub struct HttpVlm {
    client: reqwest::blocking::Client,
    endpoint: String,
    model: String,
}

impl HttpVlm {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::blocking::Client::new(),
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }
}

impl ModelProvider for HttpVlm {
    fn kind(&self) -> &'static str {
        "remote-http"
    }
}

impl VlmProvider for HttpVlm {
    fn complete(&self, system: &str, user: &str, image_png: &[u8]) -> Result<String> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(image_png);
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": [
                    {"type": "text", "text": user},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}},
                ]},
            ],
        });
        let resp: ChatResp = self
            .client
            .post(url(&self.endpoint, "/v1/chat/completions"))
            .json(&body)
            .send()?
            .error_for_status()?
            .json()?;
        Ok(resp.choices.into_iter().next().ok_or_else(|| anyhow!("no choices in response"))?.message.content)
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// 把 `http://x:8080` / `http://x:8080/` / `http://x:8080/v1` 都规整成 `http://x:8080`
/// (dp-router 的 admin 端点挂在根域,不在 `/v1` 下)。
fn derive_base_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    if let Some(stripped) = trimmed.strip_suffix("/v1") {
        stripped.to_string()
    } else {
        trimmed.to_string()
    }
}

// ── warm API types ──────────────────────────────────────────────────────────

/// `warm()` / `warm_with_options()` 的可调参数。
#[derive(Debug, Clone)]
pub struct WarmOptions {
    /// 单次轮询间隔(毫秒)。默认 500ms — 在 latency 与探测开销间折中。
    pub poll_interval_ms: u64,
    /// 总超时(秒)。默认 60s — 涵盖 GGUF 首次加载到 llama.cpp 的时间。
    pub load_timeout_s: u64,
}

impl Default for WarmOptions {
    fn default() -> Self {
        Self { poll_interval_ms: 500, load_timeout_s: 60 }
    }
}

/// `warm()` 的结果。
#[derive(Debug, Clone)]
pub struct WarmOutcome {
    /// `true` = 首次 GET /admin/models 时已 online,没触发 load。
    pub already_online: bool,
    /// `true` = 触发了 `POST /admin/models/load`(无论是否最终成功 — 看 `Result`)。
    pub load_triggered: bool,
    /// 总耗时(毫秒)。
    pub elapsed_ms: u64,
}

/// `warm()` 的错误。
#[derive(Debug, thiserror::Error)]
pub enum WarmError {
    /// 模型名在 dp-router `models_root` 下找不到,也无法加载(POST /admin/models/load 返 404)。
    /// 上层应给出明确提示:"模型 {name} 未配置在 dp-router"。
    #[error("model '{0}' not found in dp-router (not preloaded, not in models_root)")]
    ModelNotFound(String),

    /// 总超时。模型可能仍在服务端加载(下个调用仍可能成功),也可能卡死了。
    /// 上层应给出"模型加载超时,可重试"提示。
    #[error("model load timed out after {0}s (may still be loading server-side)")]
    LoadTimeout(u64),

    /// 服务端标 offline(超过 restart 预算或 spawn 失败)。**不会自愈**,上层应上报。
    #[error("model '{model}' failed to load on dp-router: {reason}")]
    LoadFailed { model: String, reason: String },

    /// 网络层错误(连接被拒、DNS 失败、超时)。
    #[error("dp-router unreachable at {0}: {1}")]
    RouterUnreachable(String, #[source] reqwest::Error),

    /// dp-router 返了非预期的 HTTP status(非 200/202/404)。
    #[error("dp-router error: status={status} for {path}: {body}")]
    RouterError { path: String, status: u16, body: String },

    /// 端点不像是 dp-router(`/admin/models` 返 404)——暖起来没意义,plain OpenAI 没这个能力。
    #[error("endpoint {0} does not expose /admin/models — warm() only works against dp-router")]
    NotDpRouter(String),

    /// 响应解析失败(dp-router 协议变了 / JSON 损坏)。
    #[error("response parse error: {0}")]
    Parse(#[source] serde_json::Error),
}

// ── dp-router admin snapshot shapes (私有) ──────────────────────────────────

#[derive(Debug, Deserialize)]
struct AdminSnapshot {
    models: Vec<AdminModel>,
}

#[derive(Debug, Deserialize)]
struct AdminModel {
    name: String,
    #[serde(default)]
    status: AdminStatus,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AdminStatus {
    Online,
    Offline,
    Starting,
    // 未知 status:serde 反序列化失败时整个模型出错——这里我们提供一个 fallback
    #[serde(other)]
    Unknown,
}

impl Default for AdminStatus {
    fn default() -> Self {
        Self::Unknown
    }
}

/// Encode PCM i16 mono → in-memory WAV bytes (16-bit PCM, no external dep).
fn pcm_to_wav(pcm: &[i16], sr: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    // RIFF header
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    // fmt chunk (PCM, mono, 16-bit)
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // audio_format = PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // num_channels = mono
    w.extend_from_slice(&sr.to_le_bytes());
    w.extend_from_slice(&(sr * 2).to_le_bytes()); // byte_rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block_align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits_per_sample
    // data chunk
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for &s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

// ── response shapes (OpenAI-compatible) ──────────────────────────────────────

#[derive(Deserialize)]
struct TranscriptionResp {
    text: String,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn wav_header_is_valid() {
        let pcm = vec![0i16; 16000]; // 1s silence @ 16kHz
        let wav = pcm_to_wav(&pcm, 16000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // data_len = 16000 * 2 = 32000
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 32000);
    }

    #[test]
    fn url_trims_trailing_slash() {
        assert_eq!(url("http://h:8000/", "/v1/"), "http://h:8000/v1/");
        assert_eq!(url("http://h:8000", "/v1/"), "http://h:8000/v1/");
    }

    #[test]
    fn derive_base_url_handles_suffixes() {
        assert_eq!(derive_base_url("http://h:8080"), "http://h:8080");
        assert_eq!(derive_base_url("http://h:8080/"), "http://h:8080");
        assert_eq!(derive_base_url("http://h:8080/v1"), "http://h:8080");
        assert_eq!(derive_base_url("http://h:8080/v1/"), "http://h:8080");
        // 不误伤其它路径
        assert_eq!(derive_base_url("http://h:8080/admin"), "http://h:8080/admin");
    }

    #[test]
    fn admin_status_parses_all_kinds() {
        assert_eq!(serde_json::from_str::<AdminStatus>("\"online\"").unwrap(), AdminStatus::Online);
        assert_eq!(serde_json::from_str::<AdminStatus>("\"offline\"").unwrap(), AdminStatus::Offline);
        assert_eq!(serde_json::from_str::<AdminStatus>("\"starting\"").unwrap(), AdminStatus::Starting);
        // 未知 status → Unknown(不报错),避免 dp-router 加新 status 时 dp-models 全挂
        assert_eq!(serde_json::from_str::<AdminStatus>("\"warming\"").unwrap(), AdminStatus::Unknown);
    }

    #[test]
    fn admin_model_with_default_status() {
        // dp-router 旧版可能没 status 字段;我们默认 Unknown(等同于 starting,继续 poll)
        let m: AdminModel = serde_json::from_value(json!({"name": "q"})).unwrap();
        assert_eq!(m.name, "q");
        assert_eq!(m.status, AdminStatus::Unknown);
    }

    // ── warm() integration tests (wiremock) ────────────────────────────────

    fn snapshot_with(name: &str, status: &str) -> serde_json::Value {
        json!({
            "router": "dp-router",
            "upstream_enabled": false,
            "models": [{"name": name, "gguf": "/x", "port": 18001, "status": status, "restarts": 0}]
        })
    }

    fn snapshot_empty() -> serde_json::Value {
        json!({"router": "dp-router", "upstream_enabled": false, "models": []})
    }

    #[test]
    fn warm_returns_already_online_when_present() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(MockServer::start());
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/admin/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(snapshot_with("qwen2.5-3b", "online")))
                .expect(1) // 一次 GET,不应触发 load
                .mount(&server)
                .await;
        });

        let llm = HttpLlm::new(server.uri(), "qwen2.5-3b");
        let out = llm.warm().unwrap();
        assert!(out.already_online);
        assert!(!out.load_triggered);
        // GET 只应调一次(无需 poll)
        // wiremock 的 expect(1) 验证调用次数
    }

    #[test]
    fn warm_triggers_load_then_polls_until_online() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(MockServer::start());
        let body_empty = snapshot_empty();
        let body_online = snapshot_with("qwen2.5-3b", "online");
        rt.block_on(async {
            // 首次 GET:空表
            Mock::given(method("GET"))
                .and(path("/admin/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body_empty.clone()))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            // POST /admin/models/load → 202
            Mock::given(method("POST"))
                .and(path("/admin/models/load"))
                .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                    "name": "qwen2.5-3b", "gguf": "/x", "port": 18002,
                    "status": "starting", "restarts": 0
                })))
                .expect(1)
                .mount(&server)
                .await;
            // 之后 GET:online
            Mock::given(method("GET"))
                .and(path("/admin/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body_online))
                .mount(&server)
                .await;
        });

        let llm = HttpLlm::new(server.uri(), "qwen2.5-3b");
        let out = llm.warm_with_options(WarmOptions {
            poll_interval_ms: 50,
            load_timeout_s: 5,
        }).unwrap();
        assert!(!out.already_online);
        assert!(out.load_triggered);
        assert!(out.elapsed_ms < 5_000);
    }

    #[test]
    fn warm_returns_model_not_found_on_404_load() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(MockServer::start());
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/admin/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(snapshot_empty()))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/admin/models/load"))
                .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                    "error": "gguf not found", "name": "ghost-99b"
                })))
                .mount(&server)
                .await;
        });

        let llm = HttpLlm::new(server.uri(), "ghost-99b");
        let err = llm.warm().unwrap_err();
        assert!(matches!(err, WarmError::ModelNotFound(ref m) if m == "ghost-99b"));
    }

    #[test]
    fn warm_returns_load_timeout_when_never_online() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(MockServer::start());
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/admin/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(snapshot_empty()))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/admin/models/load"))
                .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                    "name": "slow", "status": "starting", "port": 18003, "restarts": 0
                })))
                .mount(&server)
                .await;
        });

        let llm = HttpLlm::new(server.uri(), "slow");
        let err = llm.warm_with_options(WarmOptions {
            poll_interval_ms: 50,
            load_timeout_s: 1,
        }).unwrap_err();
        assert!(matches!(err, WarmError::LoadTimeout(1)));
    }

    #[test]
    fn warm_returns_load_failed_when_server_marks_offline() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(MockServer::start());
        rt.block_on(async {
            Mock::given(method("GET"))
                .and(path("/admin/models"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    snapshot_with("broken", "offline")
                ))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/admin/models/load"))
                .respond_with(ResponseTemplate::new(202))
                .mount(&server)
                .await;
        });

        let llm = HttpLlm::new(server.uri(), "broken");
        let err = llm.warm_with_options(WarmOptions {
            poll_interval_ms: 50,
            load_timeout_s: 5,
        }).unwrap_err();
        match err {
            WarmError::LoadFailed { model, .. } => assert_eq!(model, "broken"),
            other => panic!("expected LoadFailed, got {other:?}"),
        }
    }

    #[test]
    fn warm_errors_when_endpoint_is_not_dp_router() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(MockServer::start());
        rt.block_on(async {
            // 模拟 plain OpenAI 服务(没有 /admin/models)
            Mock::given(method("GET"))
                .and(path("/admin/models"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
        });

        let llm = HttpLlm::new(server.uri(), "any");
        let err = llm.warm().unwrap_err();
        assert!(matches!(err, WarmError::NotDpRouter(_)));
    }
}
