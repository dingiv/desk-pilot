//! router — dp-router 的 HTTP 控制面 (axum)。
//!
//! 路由:
//!
//!   POST /v1/chat/completions        → 按 `model` 字段路由;**未加载 → 按需加载**(spawn + 等就绪)再转发
//!   POST /v1/audio/transcriptions    → 同上(ASR 模型)
//!   GET  /v1/models                   → 模型目录 + 已加载模型的并集
//!   GET  /admin/models                → 目录全集 + 运行时状态(未加载=available)+ 端口/重启次数
//!   POST /admin/models/load           → 控制面:主动触发加载(SDK `warm()` 用);fire-and-forget 202
//!   GET  /health                      → dp-router 自身健康
//!
//! 按需加载语义(2026-08-30 起,启动不预载):
//!   配置 `models` 是**模型目录**("哪些模型可用"),不是"启动时加载哪些"。
//!   客户端(经 dp-models SDK 或任意 OpenAI 兼容客户端)首次请求某模型名时,
//!   数据面 `ensure_online` 触发加载:未加载 → 按目录/`models_root` 解析 GGUF →
//!   spawn 子进程 → 轮询子进程 /health 直到 online → 转发本次请求。同模型并发
//!   请求:仅首个真正 spawn(每模型一把加载锁),其余等就绪。加载中的模型对
//!   /admin/models 显示 starting,对数据面表现为"请求阻塞到就绪"。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use crate::config::{
    resolve_gguf_path, resolve_model_in_root, resolve_model_path, LocalModelConfig,
    LocalModelStatus, ModelRuntimeStatus, ModelType, RouterConfig,
};
use crate::process::{LlamaProcess, PortAllocator, ProcessMap};
use crate::upstream::UpstreamClient;

/// 全局共享状态。
pub struct AppState {
    pub config: Arc<RouterConfig>,
    pub processes: ProcessMap,
    pub upstream: Option<UpstreamClient>,
    pub http: reqwest::Client,
    /// 子进程端口分配器(共享,动态加载都从这一份取端口)。
    pub port_allocator: Arc<Mutex<PortAllocator>>,
    /// 每模型一把加载锁(模型名 → 内层 Mutex 单元):同模型并发请求只让首个真正
    /// spawn,其余等锁后进"双重检查"分支直接复用。
    pub load_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

pub type SharedState = Arc<AppState>;

/// 路由装配。
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/audio/transcriptions", post(audio_transcriptions))
        .route("/v1/models", get(list_models))
        .route("/admin/models", get(admin_list_models))
        .route("/admin/models/load", post(admin_load_model))
        .route("/health", get(health))
        .with_state(state)
}

/// 按 model 名查子进程;精确未命中时再试剥掉尾部 `.gguf` 的形式
/// (客户端习惯把完整文件名当 model 名,如 `qwen2.5-3b-instruct-q4_k_m.gguf`)。
fn find_process(
    map: &HashMap<String, Arc<RwLock<LlamaProcess>>>,
    name: &str,
) -> Option<Arc<RwLock<LlamaProcess>>> {
    map.get(name).cloned().or_else(|| {
        name.strip_suffix(".gguf")
            .and_then(|n| map.get(n))
            .cloned()
    })
}

// ── POST /v1/chat/completions ─────────────────────────────────────────────

async fn chat_completions(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // 解析 body 提取 `model` 字段(只为路由;真实 body 原样转发)。
    let req_model = match serde_json::from_slice::<Value>(&body) {
        Ok(v) => v.get("model").and_then(|m| m.as_str()).map(String::from),
        Err(_) => None,
    };
    let model_name = match req_model {
        Some(m) => m,
        None => {
            // 没有 model → 走上游(若上游未配置则 400)。
            if let Some(up) = &state.upstream {
                return forward_to_upstream(state.clone(), up.clone(), headers, body).await;
            }
            return (StatusCode::BAD_REQUEST, "missing 'model' field").into_response();
        }
    };

    // 本地:按需加载(未加载 → spawn + 等就绪;已加载 → 等健康)→ 转发。
    match ensure_online(&state, &model_name).await {
        Ok(port) => return forward_to_local(state.http.clone(), port, body).await,
        // 未知模型(目录 + models_root 都没有)→ 上游 fallback(若配置了)。
        Err((StatusCode::NOT_FOUND, _)) => {
            if let Some(up) = &state.upstream {
                info!("[dp-router] local miss for model={model_name}; forwarding to upstream");
                return forward_to_upstream(state.clone(), up.clone(), headers, body).await;
            }
            return (
                StatusCode::NOT_FOUND,
                format!("model '{model_name}' not in model catalog or models_root"),
            )
                .into_response();
        }
        Err((code, msg)) => return (code, msg).into_response(),
    }
}

async fn forward_to_local(http: reqwest::Client, port: u16, body: axum::body::Bytes) -> Response {
    let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let resp = match http.post(&url).header("content-type", "application/json").body(body).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("[dp-router] local forward failed: {e}");
            return (StatusCode::BAD_GATEWAY, format!("local forward error: {e}")).into_response();
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    if let Some(ct) = resp.headers().get("content-type") {
        out_headers.insert("content-type", ct.clone());
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("body read error: {e}")).into_response();
        }
    };
    (status, out_headers, Body::from(bytes)).into_response()
}

async fn forward_to_upstream(
    _state: SharedState,
    upstream: UpstreamClient,
    _headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    match upstream.forward_chat(body).await {
        Ok((status, ct, body)) => {
            let mut h = HeaderMap::new();
            if let Some(ct) = ct {
                if let Ok(v) = HeaderValue::from_str(&ct) {
                    h.insert("content-type", v);
                }
            }
            (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), h, Body::from(body))
                .into_response()
        }
        Err(e) => {
            warn!("[dp-router] upstream forward failed: {e}");
            (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response()
        }
    }
}

// ── POST /v1/audio/transcriptions ──────────────────────────────────────────
//
// ASR 端点(OpenAI 兼容):接收 multipart 上传(WAV 等音频 + model 字段),
// 透传到本地 llama-server 子进程的同端点(llama.cpp 原生支持,见
// thirdparty/llama.cpp/tools/server/server-context.cpp:4982)。
// 子进程负责 mmproj 多模态投影 + 转写 → 返 OpenAI 兼容 JSON,我们只透传字节。
//
// 注:本路由仅转发给本地 type=asr 子进程;fallback 到 remote upstream 的语义
// 与 /v1/chat/completions 一致(未命中本地 → 转发上游)。Upstream 必须支持
// 同样的 multipart 端点(任意 OpenAI 兼容 ASR 服务)。

async fn audio_transcriptions(
    State(state): State<SharedState>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    // 1. 解析 multipart 抽取 `model` + `file`。
    let mut model_name: Option<String> = None;
    let mut file_bytes: Option<axum::body::Bytes> = None;
    let mut file_name: Option<String> = None;
    let mut file_mime: Option<String> = None;
    let mut extra_fields: Vec<(String, String)> = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "model" => {
                model_name = field.text().await.ok();
            }
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                file_mime = field.content_type().map(|m| m.to_string());
                file_bytes = field.bytes().await.ok();
            }
            // 把 language / prompt / response_format 等额外字段转发给子进程
            _ => {
                if let Ok(t) = field.text().await {
                    extra_fields.push((name, t));
                }
            }
        }
    }
    let model_name = match model_name {
        Some(m) if !m.is_empty() => m,
        _ => return (StatusCode::BAD_REQUEST, "missing 'model' field").into_response(),
    };
    let file_bytes = match file_bytes {
        Some(b) if !b.is_empty() => b,
        _ => return (StatusCode::BAD_REQUEST, "missing 'file' field").into_response(),
    };
    let file_name = file_name.unwrap_or_else(|| "audio.wav".to_string());
    let file_mime = file_mime.unwrap_or_else(|| "audio/wav".to_string());

    // 2. 本地:按需加载(同 chat_completions)
    let port = match ensure_online(&state, &model_name).await {
        Ok(port) => port,
        // 3. 未知模型 + 有 upstream → 转发 upstream(它必须也支持 /v1/audio/transcriptions)
        Err((StatusCode::NOT_FOUND, _)) => {
            if let Some(up) = &state.upstream {
                info!("[dp-router] audio miss for model={model_name}; forwarding to upstream");
                return forward_audio_to_upstream(up.clone(), &model_name, file_bytes, &file_name, &file_mime, &extra_fields).await;
            }
            return (
                StatusCode::NOT_FOUND,
                format!("model '{model_name}' not in model catalog or models_root"),
            )
                .into_response();
        }
        Err((code, msg)) => return (code, msg).into_response(),
    };

    // 拼 multipart 转发给子进程
    let mut form = reqwest::multipart::Form::new()
        .text("model", model_name.to_string())
        .part(
            "file",
            reqwest::multipart::Part::bytes(file_bytes.to_vec())
                .file_name(file_name.to_string())
                .mime_str(&file_mime)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(file_bytes.to_vec())),
        );
    for (k, v) in extra_fields {
        form = form.text(k.clone(), v.clone());
    }

    let url = format!("http://127.0.0.1:{port}/v1/audio/transcriptions");
    let resp = match state.http.post(&url).multipart(form).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("[dp-router] audio forward failed: {e}");
            return (StatusCode::BAD_GATEWAY, format!("local forward error: {e}")).into_response();
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    if let Some(ct) = resp.headers().get("content-type") {
        out_headers.insert("content-type", ct.clone());
    }
    let raw_body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("body read error: {e}")).into_response();
        }
    };
    // llama.cpp 返 `{"type":"transcript.text.done","text":..,"usage":..}`,不是 OpenAI 标准
    // `{text}`。为保持 dp-router 对外 OpenAI 兼容,把 `text` 字段抽出包成 `{text}`。
    // 失败时原样透传(下游真 OpenAI 服务返的就是标准 `{text}`,直接走)。
    let body = normalize_asr_response(&raw_body);
    out_headers.insert("content-type", HeaderValue::from_static("application/json"));
    (status, out_headers, Body::from(body)).into_response()
}

async fn forward_audio_to_upstream(
    upstream: UpstreamClient,
    model_name: &str,
    file_bytes: axum::body::Bytes,
    file_name: &str,
    file_mime: &str,
    extra_fields: &[(String, String)],
) -> Response {
    let url = format!("{}/v1/audio/transcriptions", upstream.base_url);
    let mut form = reqwest::multipart::Form::new()
        .text("model", model_name.to_string())
        .part(
            "file",
            reqwest::multipart::Part::bytes(file_bytes.to_vec())
                .file_name(file_name.to_string())
                .mime_str(&file_mime)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(file_bytes.to_vec())),
        );
    for (k, v) in extra_fields {
        form = form.text(k.clone(), v.clone());
    }
    let mut req = upstream.http.post(&url).multipart(form);
    if let Some(key) = &upstream.api_key {
        req = req.bearer_auth(key);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("[dp-router] upstream audio forward failed: {e}");
            return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response();
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    out_headers.insert("content-type", HeaderValue::from_static("application/json"));
    let raw_body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("body read error: {e}")).into_response(),
    };
    // 同样归一化(上游可能是 mloader / OpenAI / 任意实现)
    let body = normalize_asr_response(&raw_body);
    (status, out_headers, Body::from(body)).into_response()
}

/// 归一化 ASR 响应到 OpenAI `{text}` 格式。
///
/// 输入形态容忍:
///   - `{text}` (OpenAI 标准)  → 原样
///   - `{type, text, usage}` (llama.cpp 风格) → 取 `text` 字段
///   - 其它 → 原样透传(避免破坏未知格式)
///
/// 进一步清理:llama.cpp multimodal ASR(qwen3-asr 等)的 `text` 字段总带
/// `language <lang><asr_text>` 前缀(模型的"思维外化"格式),不是 metadata
/// 而是转写内容的一部分 —— 若原样透传,aura 等客户端看到的 transcript
/// 会被噪音污染,Stage2 LLM 也只会 echo 这串前缀。归一化时剥掉,客户端拿
/// 干净的 OpenAI 标准 `{text}`。
fn normalize_asr_response(raw: &[u8]) -> Vec<u8> {
    let Ok(v) = serde_json::from_slice::<Value>(raw) else { return raw.to_vec() };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return raw.to_vec(),
    };
    // 已有 `text` 字段且无 `type` → OpenAI 标准,原样
    if obj.contains_key("text") && !obj.contains_key("type") {
        return raw.to_vec();
    }
    // 有 `text` 字段(llama.cpp 风格 `transcript.text.done`) → 抽出来包成 `{text}`
    if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
        let cleaned = strip_asr_prefix(text);
        return serde_json::json!({ "text": cleaned }).to_string().into_bytes();
    }
    raw.to_vec()
}

/// 剥 llama.cpp multimodal ASR 输出前缀:`language <lang><asr_text>...`
/// 不区分大小写;前缀缺失 → 原样。
fn strip_asr_prefix(text: &str) -> String {
    const MARKER: &str = "<asr_text>";
    let trimmed = text.trim_start();
    if !trimmed.to_ascii_lowercase().starts_with("language ") {
        return text.to_string();
    }
    match trimmed.find(MARKER) {
        Some(idx) => trimmed[idx + MARKER.len()..].to_string(),
        None => text.to_string(),
    }
}

// ── GET /v1/models ────────────────────────────────────────────────────────

async fn list_models(State(state): State<SharedState>) -> Response {
    // 模型目录(可用,按需加载)+ 已加载子进程(含 admin 动态加载的非目录模型)的并集。
    let mut names: Vec<String> = Vec::new();
    for m in &state.config.models {
        if !names.contains(&m.name) {
            names.push(m.name.clone());
        }
    }
    {
        let map = state.processes.read().await;
        for name in map.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names.sort();
    let data: Vec<Value> =
        names.iter().map(|n| json!({ "id": n, "object": "model" })).collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

// ── GET /admin/models ─────────────────────────────────────────────────────

async fn admin_list_models(State(state): State<SharedState>) -> Response {
    let mut out: Vec<LocalModelStatus> = Vec::new();
    let mut loaded: Vec<String> = Vec::new();
    {
        let map = state.processes.read().await;
        for proc_lock in map.values() {
            let p = proc_lock.read().await;
            out.push(LocalModelStatus {
                name: p.model_name.clone(),
                gguf: p.config.gguf.clone(),
                port: p.port,
                status: p.status,
                restarts: p.restarts,
            });
            loaded.push(p.model_name.clone());
        }
    }
    // 配置目录里尚未加载的模型 → available(SDK warm() 或数据面首次请求会拉起)。
    for m in &state.config.models {
        if !loaded.contains(&m.name) {
            out.push(LocalModelStatus {
                name: m.name.clone(),
                gguf: m.gguf.clone(),
                port: 0,
                status: ModelRuntimeStatus::Available,
                restarts: 0,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    #[derive(Serialize)]
    struct Resp {
        router: &'static str,
        upstream_enabled: bool,
        models: Vec<LocalModelStatus>,
    }
    Json(Resp {
        router: "dp-router",
        upstream_enabled: state.upstream.as_ref().map(|u| u.is_enabled()).unwrap_or(false),
        models: out,
    })
    .into_response()
}

// ── POST /admin/models/load ───────────────────────────────────────────────
//
// 控制面:让 SDK 主动拉起一个模型。典型用法:
//   POST /admin/models/load  body: {"name": "qwen3-asr-1.7b"}
// 服务端:
//   1. 若已在表中(任意状态) → 200,直接返当前快照(幂等)
//   2. 解析 body.gguf;未提供则在 `models_root` 下按名搜索
//   3. 都找不到 → 404 + 当前扫描到的候选(便于排查)
//   4. 都齐全 → 分配端口、spawn、注册到 routing table,立即返 202 + 状态
//      (实际加载在后台 fire-and-forget,SDK 轮询 /admin/models 直到 online 再发 chat)

#[derive(Debug, Deserialize)]
struct LoadModelRequest {
    /// 模型名(对外暴露的标识,与 /v1/chat/completions 的 `model` 字段一致)。
    name: String,
    /// 可选:显式 GGUF 路径(MODELS::/绝对路径)。未提供则按 name 在 models_root 搜索。
    gguf: Option<String>,
    /// 覆盖默认 llama-server 参数(可选)。
    context_size: Option<u32>,
    threads: Option<u32>,
    gpu_layers: Option<u32>,
    batch_size: Option<u32>,
}

#[derive(Debug, Serialize)]
struct LoadModelResponse {
    name: String,
    gguf: String,
    port: u16,
    status: ModelRuntimeStatus,
    restarts: u32,
}

async fn admin_load_model(
    State(state): State<SharedState>,
    Json(req): Json<LoadModelRequest>,
) -> Response {
    if req.name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name must not be empty").into_response();
    }

    // 幂等:已在表里 → 直接返当前快照
    {
        let map = state.processes.read().await;
        if let Some(proc_lock) = map.get(&req.name) {
            let p = proc_lock.read().await;
            return Json(LoadModelResponse {
                name: p.model_name.clone(),
                gguf: p.config.gguf.clone(),
                port: p.port,
                status: p.status,
                restarts: p.restarts,
            })
            .into_response();
        }
    }

    // 模型定义:目录(按名匹配)为基础;请求显式 gguf 时覆盖其路径。
    let mut mc = resolve_model_def(&state.config, &req.name);
    if let Some(raw) = &req.gguf {
        match resolve_model_path(raw, state.config.models_root_resolved.as_deref()) {
            Ok(p) if p.is_file() => {
                mc = Some(match mc {
                    Some(mut m) => {
                        m.gguf = p.to_string_lossy().into_owned();
                        m
                    }
                    None => LocalModelConfig {
                        name: req.name.clone(),
                        r#type: ModelType::Llm,
                        gguf: p.to_string_lossy().into_owned(),
                        mmproj: None,
                        context_size: 4096,
                        threads: 8,
                        gpu_layers: 0,
                        batch_size: 512,
                        extra_args: vec![],
                    },
                });
            }
            _ => return gguf_not_found_response(&req.name),
        }
    }
    let Some(mut mc) = mc else {
        return gguf_not_found_response(&req.name);
    };

    // 请求参数覆盖(目录定义 < 请求)
    mc.context_size = req.context_size.unwrap_or(mc.context_size);
    mc.threads = req.threads.unwrap_or(mc.threads);
    mc.gpu_layers = req.gpu_layers.unwrap_or(mc.gpu_layers);
    mc.batch_size = req.batch_size.unwrap_or(mc.batch_size);

    match spawn_with_config(&state, &mc).await {
        Ok(_port) => {
            let snapshot = {
                let map = state.processes.read().await;
                match find_process(&map, &mc.name) {
                    Some(pl) => {
                        let p = pl.read().await;
                        LoadModelResponse {
                            name: p.model_name.clone(),
                            gguf: p.config.gguf.clone(),
                            port: p.port,
                            status: p.status,
                            restarts: p.restarts,
                        }
                    }
                    None => LoadModelResponse {
                        name: mc.name.clone(),
                        gguf: mc.gguf.clone(),
                        port: 0,
                        status: ModelRuntimeStatus::Starting,
                        restarts: 0,
                    },
                }
            };
            info!(
                "[dp-router] dynamic load accepted: name={} port={} gguf={}",
                snapshot.name, snapshot.port, snapshot.gguf
            );
            (StatusCode::ACCEPTED, Json(snapshot)).into_response()
        }
        Err((code, msg)) => (code, msg).into_response(),
    }
}

fn gguf_not_found_response(name: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": "gguf not found",
            "name": name,
            "hint": "在请求里提供 gguf 路径,把模型放到 models_root 目录下,或加入配置 models 目录"
        })),
    )
        .into_response()
}

// ── 按需加载核心 ───────────────────────────────────────────────────────────

/// 数据面等模型就绪的上限(秒)。覆盖 CPU 冷加载的慢场景;超时返 503,
/// 健康循环仍在后台工作,客户端稍后重试即可。
const LOAD_WAIT_TIMEOUT_S: u64 = 180;

/// 配置目录里按名查模型定义(精确 → 剥尾部 `.gguf` 再试)。
fn resolve_model_def(cfg: &RouterConfig, name: &str) -> Option<LocalModelConfig> {
    cfg.models
        .iter()
        .find(|m| m.name == name)
        .or_else(|| {
            name.strip_suffix(".gguf")
                .and_then(|n| cfg.models.iter().find(|m| m.name == n))
        })
        .cloned()
}

/// 进程表查模型端口(精确 → 剥 `.gguf`;与 [`find_process`] 同语义)。
async fn lookup_port(state: &SharedState, name: &str) -> Option<u16> {
    let map = state.processes.read().await;
    let pl = find_process(&map, name)?;
    let port = pl.read().await.port;
    Some(port)
}

/// 分配端口 + spawn + 注册。不等就绪:就绪由 fire-and-forget 的 wait_ready
/// task 驱动(数据面在 [`wait_online`] 里独立等,两条路径互不依赖)。
async fn spawn_with_config(state: &SharedState, mc: &LocalModelConfig) -> Result<u16, (StatusCode, String)> {
    let port = {
        let mut alloc = state.port_allocator.lock().await;
        alloc.next().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "port range exhausted: 没有空闲内部端口加载模型".to_string(),
            )
        })?
    };
    let gguf = resolve_gguf_path(&mc.gguf)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("gguf resolve failed: {e}")))?;
    if !gguf.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("gguf file not found: {}", gguf.display()),
        ));
    }
    let mut proc = LlamaProcess::new(mc.clone(), port, state.http.clone());
    proc.spawn(&state.config.llama_server.path, &gguf, &state.config.llama_server.env)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn llama-server failed: {e}")))?;
    let proc_lock = Arc::new(RwLock::new(proc));
    state.processes.write().await.insert(mc.name.clone(), proc_lock.clone());
    tokio::spawn(async move {
        let mut p = proc_lock.write().await;
        let _ = p.wait_ready(LOAD_WAIT_TIMEOUT_S).await;
    });
    Ok(port)
}

/// 等模型变 online(有界);超时返 503。
async fn wait_online(state: &SharedState, name: &str, port: u16) -> Result<u16, (StatusCode, String)> {
    let deadline = Instant::now() + Duration::from_secs(LOAD_WAIT_TIMEOUT_S);
    loop {
        let status = {
            let map = state.processes.read().await;
            match find_process(&map, name) {
                Some(pl) => {
                    let s = pl.write().await.status;
                    s
                }
                None => ModelRuntimeStatus::Offline,
            }
        };
        if status == ModelRuntimeStatus::Online {
            return Ok(port);
        }
        if Instant::now() >= deadline {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "model '{name}' not ready after {LOAD_WAIT_TIMEOUT_S}s (status={status:?}); \
                     健康循环仍在重试,请稍后重试请求"
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 数据面入口:确保模型 online 并返回其端口。
///
/// - 已加载且 online → 直接返回
/// - 已加载但未就绪(loading / 重启中)→ 等就绪(有界)
/// - 未加载 → 解析定义(目录 → models_root 按名搜索)→ 每模型加载锁下 spawn → 等就绪
/// - 找不到 → 404(调用方决定是否 fallback upstream)
async fn ensure_online(state: &SharedState, name: &str) -> Result<u16, (StatusCode, String)> {
    // 1) 已在进程表 → 等它变 online
    if let Some(port) = lookup_port(state, name).await {
        return wait_online(state, name, port).await;
    }

    // 2) 未加载 → 解析模型定义:配置目录(按名)→ models_root(按名搜索)
    let mc = match resolve_model_def(&state.config, name) {
        Some(mc) => mc,
        None => match state
            .config
            .models_root_resolved
            .as_deref()
            .and_then(|root| resolve_model_in_root(name, root))
        {
            Some(gguf) => LocalModelConfig {
                name: name.to_string(),
                r#type: ModelType::Llm,
                gguf: gguf.to_string_lossy().into_owned(),
                mmproj: None,
                context_size: 4096,
                threads: 8,
                gpu_layers: 0,
                batch_size: 512,
                extra_args: vec![],
            },
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("model '{name}' not in model catalog or models_root"),
                ))
            }
        },
    };

    // 3) 每模型一把加载锁:同模型并发请求只让首个真正 spawn,其余拿锁后
    //    进双重检查分支直接复用。
    let lock = {
        let mut locks = state.load_locks.lock().await;
        locks.entry(mc.name.clone()).or_default().clone()
    };
    let _guard = lock.lock().await;

    // 4) 双重检查:并发请求可能已 spawn
    if let Some(port) = lookup_port(state, &mc.name).await {
        return wait_online(state, &mc.name, port).await;
    }

    // 5) spawn + 等就绪
    let port = spawn_with_config(state, &mc).await?;
    wait_online(state, &mc.name, port).await
}

// ── GET /health ───────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

// ── health-check 后台 task ────────────────────────────────────────────────

/// 定期检查所有子进程 `/health`;失败 → 触发重启(由 [`LlamaProcess::restart`] 指数退避)。
pub fn spawn_health_loop(state: SharedState) {
    let interval = state.config.llama_server.health_check_interval_s;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval));
        ticker.tick().await; // skip immediate
        loop {
            ticker.tick().await;
            run_once(&state).await;
        }
    });
}

/// Online 状态连续健康检查失败多少次才重建(5s 间隔 × 3 = 15s 持续不健康)。
/// 单次瞬时失败(生成繁忙 / 高负载下 5s 超时)不再触发 SIGKILL — 2026-08-30 事故根因:
/// 旧逻辑单次失败即杀进程,健康子进程被反复误杀,日志呈现为"无输出崩溃"。
const HEALTH_STRIKES_ONLINE: u32 = 3;
/// Offline 状态(进程存活但无响应)容忍多少次失败才重建(约 60s — 覆盖慢速冷加载)。
const HEALTH_STRIKES_OFFLINE: u32 = 12;

async fn run_once(state: &SharedState) {
    // 先收集句柄,探活/重启全程不持有进程表锁(旧逻辑持读锁做秒级探活,
    // 会阻塞 /v1/models、/admin/models 和 chat 路由的查表)。
    let procs: Vec<Arc<RwLock<LlamaProcess>>> = {
        let map = state.processes.read().await;
        map.values().cloned().collect()
    };

    let llama_path = state.config.llama_server.path.clone();
    let ll_cfg = state.config.llama_server.clone();

    for proc_lock in procs {
        let mut proc = proc_lock.write().await;
        match proc.status {
            ModelRuntimeStatus::Online => {
                // 子进程真的退出(崩溃)→ 立即重启,无需攒失败次数
                if !proc.check_alive() {
                    warn!("[dp-router] process exited: model={}", proc.model_name);
                    proc.status = ModelRuntimeStatus::Offline;
                    do_restart(&mut proc, &llama_path, &ll_cfg).await;
                    continue;
                }
                if proc.health_check().await {
                    proc.failed_checks = 0;
                    proc.budget_recovery();
                } else {
                    proc.failed_checks += 1;
                    if proc.failed_checks < HEALTH_STRIKES_ONLINE {
                        continue;
                    }
                    warn!(
                        "[dp-router] health check failed {}x in a row, rebuilding: model={}",
                        proc.failed_checks, proc.model_name
                    );
                    proc.failed_checks = 0;
                    proc.status = ModelRuntimeStatus::Offline;
                    do_restart(&mut proc, &llama_path, &ll_cfg).await;
                }
            }
            ModelRuntimeStatus::Starting => {
                // 就绪由 wait_ready task 负责;这里只兜"加载期间子进程死亡"
                if !proc.check_alive() {
                    proc.status = ModelRuntimeStatus::Offline;
                    do_restart(&mut proc, &llama_path, &ll_cfg).await;
                }
            }
            ModelRuntimeStatus::Available => {
                // 进程表里不会出现(仅 /admin/models 展示用);防御性跳过
                continue;
            }
            ModelRuntimeStatus::Offline => {
                if !proc.check_alive() {
                    // 进程确实没了 → 带预算重启(冷却期跳过)
                    do_restart(&mut proc, &llama_path, &ll_cfg).await;
                    continue;
                }
                if proc.health_check().await {
                    // 自愈:之前被误判(或慢速冷加载刚完成)→ 直接回 Online,
                    // 不杀不重建(这正是 2026-08-30 事故中"被判死"的存活进程)
                    info!("[dp-router] model recovered: model={}", proc.model_name);
                    proc.status = ModelRuntimeStatus::Online;
                    proc.online_since = Some(std::time::Instant::now());
                    proc.failed_checks = 0;
                } else {
                    // 进程活着但持续无响应:可能还在慢速加载,容忍一段时间
                    // 后才杀掉重建
                    proc.failed_checks += 1;
                    if proc.failed_checks < HEALTH_STRIKES_OFFLINE {
                        continue;
                    }
                    warn!(
                        "[dp-router] process unresponsive {}x in a row, killing: model={}",
                        proc.failed_checks, proc.model_name
                    );
                    proc.failed_checks = 0;
                    do_restart(&mut proc, &llama_path, &ll_cfg).await;
                }
            }
        }
    }
}

/// 解析 gguf 并重启;解析失败仅告警(下轮再试)。
async fn do_restart(
    proc: &mut LlamaProcess,
    llama_path: &std::path::Path,
    ll_cfg: &crate::config::LlamaServerConfig,
) {
    match resolve_gguf_path(&proc.config.gguf) {
        Ok(gguf) => {
            let _ = proc.restart(llama_path, &gguf, ll_cfg).await;
        }
        Err(e) => warn!("[dp-router] gguf resolve failed: {e}"),
    }
}

/// 关闭所有子进程(graceful)。
pub async fn shutdown_all(processes: &ProcessMap) {
    let map = processes.read().await;
    for proc_lock in map.values() {
        let mut p = proc_lock.write().await;
        p.shutdown().await;
    }
}

/// 用于辅助 `list_models` 输出稳定顺序(按 name 排序)。
#[allow(dead_code)]
fn _sort_models(mut v: Vec<LocalModelStatus>) -> Vec<LocalModelStatus> {
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// 内部辅助:把 `processes: ProcessMap` 在 admin 查询里复制后保持顺序无关的视图。
#[allow(dead_code)]
fn _as_map(v: &[LocalModelStatus]) -> HashMap<String, ModelRuntimeStatus> {
    v.iter().map(|s| (s.name.clone(), s.status)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_asr_prefix_removes_qwen3_asr_thinking_format() {
        assert_eq!(
            strip_asr_prefix("language Chinese<asr_text>開放時間：早上九點至下午五點。"),
            "開放時間：早上九點至下午五點。"
        );
        assert_eq!(
            strip_asr_prefix("language Chinese<asr_text>不行。"),
            "不行。"
        );
        assert_eq!(
            strip_asr_prefix("language None<asr_text>真的吗？"),
            "真的吗？"
        );
    }

    #[test]
    fn strip_asr_prefix_handles_leading_whitespace_and_case() {
        assert_eq!(
            strip_asr_prefix("   language Chinese<asr_text>hello"),
            "hello"
        );
        assert_eq!(
            strip_asr_prefix("LANGUAGE ZH<asr_text>hi"), // 大写 + 中文 lang
            "hi"
        );
    }

    #[test]
    fn strip_asr_prefix_passthrough_when_no_marker() {
        // 没前缀 → 原样
        assert_eq!(strip_asr_prefix("干净的纯文本"), "干净的纯文本");
        // 有 `language ` 但缺 `<asr_text>` → 原样(避免误伤未知格式)
        assert_eq!(
            strip_asr_prefix("language Chinese:hello"),
            "language Chinese:hello"
        );
    }

    #[test]
    fn normalize_asr_response_strips_qwen3_asr_prefix() {
        // llama.cpp 风格 + 前缀 → 应剥成 OpenAI 标准 `{text: <clean>}`
        let raw = serde_json::json!({
            "type": "transcript.text.done",
            "text": "language Chinese<asr_text>不行。",
            "usage": {"type": "tokens"}
        })
        .to_string()
        .into_bytes();
        let out: Value = serde_json::from_slice(&normalize_asr_response(&raw)).unwrap();
        assert_eq!(out["text"], "不行。");

        // 干净文本(无前缀)→ 不动
        let raw = serde_json::json!({"text": "already clean"}).to_string().into_bytes();
        let out: Value = serde_json::from_slice(&normalize_asr_response(&raw)).unwrap();
        assert_eq!(out["text"], "already clean");
    }
}