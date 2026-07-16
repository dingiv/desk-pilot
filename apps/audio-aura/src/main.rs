//! aura-daemon — the audio-aura binary entry point. Composes the [`Pipeline`] (Stage1→Stage2),
//! runs an in-process **Stage3 rule trigger**, and exposes a socket for the desktop-pet / web UI:
//! a scout-connection toggle (`POST /api/control/scout`), a live SSE stream of Stage1 Interim
//! partials + Final utterances (`GET /api/stream`), and status (`GET /api/status`).
//!
//! Threading: the Pipeline runs on a dedicated **std thread** (it blocks forever); the axum
//! socket runs on a multi-thread tokio runtime on the main thread. The Pipeline's `on_turn`
//! callback serializes each [`TurnEvent`] to owned JSON and publishes it on a
//! `broadcast::Sender<Value>` — the SSE handler subscribes and streams it. (TurnEvent borrows, so
//! it can't cross the channel directly; the JSON step also clones the borrowed fields.)
//!
//! Run: cargo run -p aura-daemon --features asr,cuda -- 127.0.0.1:7879

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use audio_aura_agent::{AddHotwordTool, HotwordManager, SharedHotwordManager, Tool};
use audio_aura_asr::executor::{OnnxStage1Executor, Stage1Config};
use audio_aura_core::{Pipeline, TurnEvent};
use audio_aura_router::calibrator::RouterStage2Calibrator;
use audio_aura_router::RouterEngine;

const BASE: &str = "/workspaces/gui_agent/audio-aura/native";

/// Shared daemon state surfaced over the socket.
/// In-memory audio clip store (seq → PCM), for the web UI's playback feature.
/// Bounded to the last 30 segments (~minutes of speech) to cap memory.
type AudioClips = Arc<Mutex<HashMap<u64, Vec<i16>>>>;

#[derive(Clone)]
struct DaemonState {
    hotwords: Arc<Mutex<Vec<String>>>,
    /// Scout-connection toggle (shared with Stage1Executor's ingest + run loop).
    active: Arc<AtomicBool>,
    /// Event bus bridging the Pipeline thread → SSE clients.
    bus: broadcast::Sender<Value>,
    /// PCM audio clips keyed by utterance seq (for `GET /api/audio/:seq` playback).
    audio_clips: AudioClips,
}

impl DaemonState {
    fn emit(&self, ev: Value) {
        let _ = self.bus.send(ev); // Err only when there are no receivers (fine).
    }
}

fn main() -> Result<()> {
    let scout_addr = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SCOUT_ADDR").ok())
        .unwrap_or_else(|| "127.0.0.1:7878".to_string());
    let port: u16 = std::env::var("AURA_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(9091);
    let stage3_on = std::env::var("STAGE3_OFF").is_err();

    // Connection toggle + event bus, shared across the Pipeline thread + socket handlers.
    let active = Arc::new(AtomicBool::new(true));
    let (bus, _rx) = broadcast::channel::<Value>(1024);

    // Shared hotword store = the Stage3→Stage2 feedback channel (starts empty; Stage3 fills it).
    let hotwords: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mgr: Arc<dyn HotwordManager> = Arc::new(SharedHotwordManager::new(Arc::clone(&hotwords)));
    let tool = AddHotwordTool::new(Arc::clone(&mgr));

    // Audio clip store for playback.
    let audio_clips: AudioClips = Arc::new(Mutex::new(HashMap::new()));

    eprintln!("[aura-daemon] loading Stage1 (ONNX) + Stage2 (Qwen router) …");
    let mut cfg = Stage1Config::new(scout_addr.clone());
    cfg.active = Arc::clone(&active); // share the toggle with the executor
    let s1 = OnnxStage1Executor::new(cfg)?;
    let router = RouterEngine::load_default("Qwen3-1.7B-Q8_0.gguf")?;
    let _ = router.route_blocking("你好", None, &[]); // HF warmup
    let s2 = RouterStage2Calibrator::new(router, Arc::clone(&hotwords));

    // ── Pipeline on a dedicated std thread ── it bridges each TurnEvent to the event bus as
    //    owned JSON (seq tracked so the live (in-progress) item's id is known for interims).
    let current_seq = Arc::new(AtomicU64::new(1));
    {
        let bus = bus.clone();
        let tool = tool.clone();
        let current_seq = Arc::clone(&current_seq);
        let audio_clips = Arc::clone(&audio_clips);
        thread::Builder::new()
            .name("aura-pipeline".into())
            .spawn(move || {
                Pipeline::new(s1, s2).run(move |ev| {
                    match ev {
                        TurnEvent::Interim { partial, at_s } => {
                            let seq = current_seq.load(Ordering::Relaxed);
                            eprintln!("  …流式 @{at_s:.1}s: {partial}");
                            bus.send(json!({ "type":"interim", "seq":seq, "partial":partial, "at_s":at_s })).ok();
                        }
                        TurnEvent::Final { utterance: u, decision: d, route_ms } => {
                            eprintln!(
                                "▶ #{} @{:.1}s [{}, {:.0}ms]  整流: {}",
                                u.seq, u.at_s, d.intent, route_ms, d.calibrated_text
                            );
                            if stage3_on {
                                stage3_rule_trigger(&tool, &d.calibrated_text);
                            }
                            // Store the PCM for playback (bound to last 30 clips).
                            {
                                let mut clips = audio_clips.lock().unwrap();
                                if clips.len() >= 30 {
                                    let oldest = *clips.keys().min().unwrap();
                                    clips.remove(&oldest);
                                }
                                clips.insert(u.seq, u.pcm.clone());
                            }
                            bus.send(json!({
                                "type":"final",
                                "seq": u.seq,
                                "raw_text": &u.raw_text,
                                "streaming_text": &u.streaming_text,
                                "calibrated": &d.calibrated_text,
                                "intent": &d.intent,
                                "reply": &d.reply,
                                "route_ms": route_ms,
                            })).ok();
                            current_seq.store(u.seq + 1, Ordering::Relaxed);
                        }
                    }
                });
            })?;
    }

    // ── Socket on the main thread's tokio runtime ──
    let state = DaemonState { hotwords: Arc::clone(&hotwords), active: Arc::clone(&active), bus, audio_clips };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("aura-socket")
        .build()?;
    eprintln!("[aura-daemon] socket: http://127.0.0.1:{port}  (/health /api/status /api/stream /api/control/scout /context)");
    eprintln!("[aura-daemon] pipeline running on bg thread (scout {scout_addr}/audio). stage3 trigger = {stage3_on}. Ctrl-C 结束.");
    rt.block_on(serve_socket(state, port));
    Ok(())
}

/// In-process Stage3 rule trigger (temporary; desktop-pet replaces it). Extracts uppercase-latin
/// proper-noun candidates from the calibrated text and adds them as hotwords — locking in Stage2's
/// corrections so future turns are reinforced.
fn stage3_rule_trigger(tool: &AddHotwordTool, text: &str) {
    for tok in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.len() < 2 || !tok.chars().any(|c| c.is_ascii_uppercase()) {
            continue;
        }
        if let Ok(out) = tool.invoke(&json!({ "word": tok })) {
            if out["added"].as_bool() == Some(true) {
                eprintln!("   [stage3] 规则触发器加词: {tok}");
            }
        }
    }
}

async fn serve_socket(state: DaemonState, port: u16) {
    // Production: the daemon also serves the built SPA (same origin — no proxy needed). Resolve
    // dist/ from the workspace root (BASE minus "/native") so it's independent of the daemon's
    // cwd; override with AURA_WEB_DIST. In dev Vite serves the page (dist may be absent → 404,
    // harmless).
    let ws_root = BASE.strip_suffix("/native").unwrap_or(BASE);
    let dist_dir = std::env::var("AURA_WEB_DIST").unwrap_or_else(|_| format!("{ws_root}/dist"));
    let static_spa = ServeDir::new(&dist_dir).fallback(ServeFile::new(format!("{dist_dir}/index.html")));
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/status", get(status_handler))
        .route("/api/control/scout", post(control_scout))
        .route("/api/stream", get(stream_asr))
        .route("/api/audio/{seq}", get(audio_handler))
        .route("/context", get(context_handler))
        // remaining stubs (speaker / results / annotate) — out of scope this round
        .route("/control/speaker", post(control_stub))
        .route("/results", get(results_stub))
        .route("/annotate", post(annotate_stub))
        .fallback_service(static_spa)
        .layer(CorsLayer::permissive())
        .with_state(state);
    let listener = match tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[aura-daemon] socket bind :{port} failed: {e}");
            return;
        }
    };
    eprintln!("[aura-daemon] socket listening on :{port}");
    let _ = axum::serve(listener, app).await;
}

async fn health(State(_s): State<DaemonState>) -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Current scout-connection state (for the toggle's initial render).
async fn status_handler(State(s): State<DaemonState>) -> Json<Value> {
    Json(json!({ "connected": s.active.load(Ordering::Relaxed) }))
}

/// Toggle aura's OWN connection to scout (does NOT kill scout). Body: `{"enabled": bool}`.
async fn control_scout(State(s): State<DaemonState>, body: Json<Value>) -> Json<Value> {
    let enabled = body.get("enabled").and_then(|v| v.as_bool());
    let next = match enabled {
        Some(v) => v,
        None => !s.active.load(Ordering::Relaxed), // no arg → flip
    };
    s.active.store(next, Ordering::Relaxed);
    s.emit(json!({ "type": "status", "connected": next }));
    Json(json!({ "connected": next }))
}

/// SSE stream of Stage1 events: hello → (interim | final | status)*. Each event is one
/// `data: <json>\n\n` frame. The bridge from the Pipeline thread is the broadcast channel.
async fn stream_asr(
    State(s): State<DaemonState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = s.bus.subscribe();
    let hello = tokio_stream::once(Ok::<_, Infallible>(
        Event::default().data(json!({ "type": "hello" }).to_string()),
    ));
    let live = BroadcastStream::new(rx).map(|res| match res {
        Ok(v) => Ok(Event::default().data(v.to_string())),
        Err(_) => Ok(Event::default().comment("lagged")),
    });
    Sse::new(hello.chain(live)).keep_alive(KeepAlive::default())
}

async fn context_handler(State(s): State<DaemonState>) -> Json<Value> {
    Json(json!({ "hotwords": s.hotwords.lock().unwrap().clone() }))
}

/// `GET /api/audio/:seq` — serve the raw PCM of utterance `seq` as a WAV file for playback.
async fn audio_handler(
    State(s): State<DaemonState>,
    Path(seq): Path<u64>,
) -> impl IntoResponse {
    let pcm = s.audio_clips.lock().unwrap().get(&seq).cloned();
    match pcm {
        Some(pcm) => {
            let wav = audio_aura_asr::wav::wav_bytes(&pcm, 16000);
            (
                [(axum::http::header::CONTENT_TYPE, "audio/wav")],
                wav,
            )
                .into_response()
        }
        None => (axum::http::StatusCode::NOT_FOUND, "audio clip not found").into_response(),
    }
}

async fn control_stub() -> Json<Value> {
    Json(json!({ "stub": true, "todo": "speaker control + runtime config" }))
}
async fn results_stub() -> Json<Value> {
    Json(json!({ "stub": true, "todo": "stage1/2 results + rolling context window" }))
}
async fn annotate_stub() -> Json<Value> {
    Json(json!({ "stub": true, "todo": "accept user corrections → progressive-improvement dataset" }))
}
