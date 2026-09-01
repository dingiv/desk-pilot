//! 路由层(router)—— axum Router 组装 + 全部 HTTP/SSE handler。
//! handler 只做:参数/Body 提取 → 调 [`DaemonState`](service 层)API → JSON/SSE 整形;
//! 业务与持久化不在这层。SSE 长连接的订阅守卫(SubGuard/Guarded)也归这里。

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info};

use crate::service::DaemonState;

const BASE: &str = "/workspaces/gui_agent/audio-aura/native";

/// Build the socket router + bind + serve. Production: the daemon also serves the built SPA
/// (same origin — no proxy needed). Resolve dist/ from the workspace root (BASE minus
/// "/native") so it's independent of the daemon's cwd; override with `web_dist` (aura.yaml).
/// In dev Vite serves the page (dist may be absent → 404, harmless).
pub(crate) async fn serve_socket(state: DaemonState, bind_addr: String, port: u16, web_dist: Option<String>) {
    // TODO: 硬编码了 static 文件路径，使用 FileLoader 提供的机制来处理
    let ws_root = BASE.strip_suffix("/native").unwrap_or(BASE);
    let dist_dir = web_dist.unwrap_or_else(|| format!("{ws_root}/dist"));
    let static_spa = ServeDir::new(&dist_dir).fallback(ServeFile::new(format!("{dist_dir}/index.html")));
    let app = Router::new()
        .route("/health", get(health))
        // ── the snapshot-sync contract ──
        .route("/api/state", get(state_handler))           // full AuraStateView snapshot
        .route("/api/stream", get(stream_asr))             // control plane: hello → state_changed* (throttled)
        .route("/api/asr_stream", get(asr_stream))         // data plane: hello → recognition sentences* (pushed)
        // ── actions (each mutates state → bumps version → next SSE tick pings) ──
        .route("/api/control/scout", post(control_scout))
        .route("/api/correct", post(correction_handler))
        // 主动归档(IME 分字符 `'` = "我说完了"):识别域动作,不 bump version ——
        // 归档产生的段落事件走数据面 /api/asr_stream 推送。
        .route("/api/control/flush", post(flush_handler))
        // ── binary / queries ──
        .route("/api/audio/{seq}", get(audio_handler))
        .route("/api/recordings", get(recordings_handler))
        // 全量历史识别消息(最近定稿,最旧 → 最新)—— 重连后 swift-ime 拉一次
        // 同步本地 voice_state,补齐断连期间 aura 侧已定稿的句子。
        .route("/api/results", get(results_handler))
        .fallback_service(static_spa)
        .layer(CorsLayer::permissive())
        .with_state(state);
    let listener = match tokio::net::TcpListener::bind(format!("{bind_addr}:{port}")).await {
        Ok(l) => l,
        Err(e) => {
            error!(port, error = %e, "socket bind failed");
            return;
        }
    };
    info!(port, "socket listening");
    let _ = axum::serve(listener, app).await;
}

async fn health(State(_s): State<DaemonState>) -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// `GET /api/state` — the complete [`AuraStateView`] snapshot. The frontend fetches this on mount
/// and again whenever `/api/stream` pings `state_changed`. One source of truth for all display.
async fn state_handler(State(s): State<DaemonState>) -> Json<audio_aura_agent::AuraStateView> {
    Json(s.snapshot())
}

/// Toggle aura's OWN connection to scout (does NOT kill scout). Body: `{"enabled": bool}`.
async fn control_scout(State(s): State<DaemonState>, body: Json<Value>) -> Json<Value> {
    let enabled = body.get("enabled").and_then(|v| v.as_bool());
    let next = s.set_scout(enabled);
    Json(json!({ "connected": next }))
}

/// `POST /api/control/flush` — 主动归档当前开放段落(IME 分字符 `'` 触发)。
/// 置位即返:Stage1 消费循环(≤50ms 唤醒)负责消费标记并立即整段 batch。
/// 说话中(EOS 未到)挂起重试;无段落时标记被消费(空按)。
async fn flush_handler(State(s): State<DaemonState>) -> Json<Value> {
    s.request_flush();
    Json(json!({ "flush": true }))
}

/// SSE subscription params: `?state_changed_frequency=<ms>` — the minimum interval between
/// `state_changed` pings (floor 250 ms = max 4 Hz). The frontend renders at its own pace and may
/// skip pings; this just caps wire traffic.
#[derive(Debug, Deserialize)]
struct StreamParams {
    state_changed_frequency: Option<u64>,
}

/// `GET /api/stream?state_changed_frequency=400` — SSE: `hello`, then a `state_changed` ping each
/// tick (at the client's rate, floor 250 ms) WHENEVER the global `version` advanced since the last
/// tick the connection saw. No data is carried — the client re-GETs /api/state. Trailing-edge
/// guaranteed: a change is always reported within one tick (a paused state syncs, never stuck).
async fn stream_asr(
    State(s): State<DaemonState>,
    Query(q): Query<StreamParams>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    // 长连接订阅登记(首个客户端连上且 idle → 恢复识别; 断开时 -1)。
    let guard = SubGuard::subscribe(s.clone());
    let freq_ms = q.state_changed_frequency.unwrap_or(400).max(250);
    let version = Arc::clone(&s.version);
    let last_seen = Arc::new(AtomicU64::new(version.load(Ordering::Acquire)));

    let hello = tokio_stream::once(Ok::<_, Infallible>(
        Event::default().data(json!({ "type": "hello" }).to_string()),
    ));
    let pings = IntervalStream::new(tokio::time::interval(Duration::from_millis(freq_ms))).filter_map(
        move |_| {
            // Sync closure — AtomicU64 loads need no await. Emits one state_changed per tick iff
            // the global version advanced since this connection last looked.
            let cur = version.load(Ordering::Acquire);
            let prev = last_seen.load(Ordering::Acquire);
            if cur > prev {
                last_seen.store(cur, Ordering::Release);
                Some(Ok::<_, Infallible>(
                    Event::default().data(json!({ "type": "state_changed" }).to_string()),
                ))
            } else {
                None
            }
        },
    );
    Sse::new(Guarded { inner: hello.chain(pings), _guard: guard }).keep_alive(KeepAlive::default())
}

/// `GET /api/asr_stream` — the DATA plane: pushes each recognition sentence directly to the
/// subscriber (low-latency, every event — not throttled, unlike the control-plane `/api/stream`).
/// One `data: <AsrEvent json>\n\n` frame per recognition event (StreamFragment / BatchSentence
/// / BatchParagraph / SentenceCalibration / ParagraphCalibration / Correction). Late/lagged
/// subscribers get a `lagged` comment (broadcast backlog overflowed) and keep going.
async fn asr_stream(State(s): State<DaemonState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    // 长连接订阅登记(首个客户端连上且 idle → 恢复识别; 断开时 -1)。
    let guard = SubGuard::subscribe(s.clone());
    let rx = s.asr_events.subscribe();
    let hello = tokio_stream::once(Ok::<_, Infallible>(
        Event::default().data(json!({ "type": "hello" }).to_string()),
    ));
    let live = BroadcastStream::new(rx).map(|res| match res {
        Ok(seg) => Ok(Event::default().data(
            serde_json::to_string(&seg).unwrap_or_else(|_| "{}".into()),
        )),
        Err(_) => Ok(Event::default().comment("lagged")),
    });
    Sse::new(Guarded { inner: hello.chain(live), _guard: guard }).keep_alive(KeepAlive::default())
}

/// `GET /api/audio/:paragraph_id` — serve the settled paragraph's WAV for playback. The archive
/// resolves transparently: hot tier first, then the flushed file on disk.
async fn audio_handler(
    State(s): State<DaemonState>,
    Path(paragraph_id): Path<u64>,
) -> impl IntoResponse {
    match s.wav(paragraph_id) {
        Some(wav) => {
            ([(axum::http::header::CONTENT_TYPE, "audio/wav")], wav).into_response()
        }
        None => (axum::http::StatusCode::NOT_FOUND, "audio clip not found").into_response(),
    }
}

/// `GET /api/recordings` — list all known clips (hot + flushed), ascending seq.
async fn recordings_handler(State(s): State<DaemonState>) -> Json<Value> {
    Json(json!({ "recordings": s.recordings() }))
}

/// `GET /api/results` — 最近定稿的识别文本(最旧 → 最新)。数据面(`/api/asr_stream`)
/// 是 append-only broadcast,重连后的新订阅者**不会收到历史句**;本接口补足全量
/// 历史,供客户端重连后同步本地状态。
async fn results_handler(State(s): State<DaemonState>) -> Json<Value> {
    let recs = s.recent_turns();
    let texts: Vec<serde_json::Value> = recs
        .iter()
        .map(|r| {
            json!({
                "paragraph_id": r.paragraph_id,
                "unix_ms": r.unix_ms,
                "raw_text": r.raw_text,
                "streaming_text": r.streaming_text,
                "calibrated": r.calibrated,
            })
        })
        .collect();
    Json(json!({ "results": texts }))
}

/// `POST /api/correct {paragraph_id, raw, corrected}` — record a user correction for a settled
/// paragraph: push to the Stage2 correction store, flag the timeline entry `corrected_by_user`,
/// and bump `version` so clients re-fetch and see the badge.
async fn correction_handler(State(s): State<DaemonState>, body: Json<Value>) -> Json<Value> {
    let raw = body.get("raw").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let corrected = body.get("corrected").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let paragraph_id = body.get("paragraph_id").and_then(|v| v.as_u64()).unwrap_or(0);
    if !s.add_correction(&raw, &corrected, paragraph_id) {
        return Json(json!({ "ok": false, "error": "raw and corrected required" }));
    }
    Json(json!({ "ok": true }))
}

/// 订阅守卫:连接时 subscriber +1(0→1 且 idle 时自动恢复); 断开(Drop)时 -1。
struct SubGuard {
    state: DaemonState,
}
impl SubGuard {
    fn subscribe(state: DaemonState) -> SubGuard {
        let was_zero = state.subscribers.fetch_add(1, Ordering::SeqCst) == 0;
        if was_zero && state.idle.load(Ordering::Relaxed) {
            state.resume(); // 首个客户端连上 → 从深度睡眠恢复
        }
        SubGuard { state }
    }
}
impl Drop for SubGuard {
    fn drop(&mut self) {
        self.state.subscribers.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 持有订阅守卫的流:守卫随流一起 drop, 保证断开时 subscriber 减一。
struct Guarded<S> {
    inner: S,
    _guard: SubGuard,
}
impl<S: tokio_stream::Stream + Unpin> tokio_stream::Stream for Guarded<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}
