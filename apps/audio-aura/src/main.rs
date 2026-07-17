//! aura-daemon — the audio-aura binary entry point. Composes the [`Pipeline`] (Stage1→Stage2),
//! runs an in-process **Stage3 rule trigger**, and exposes a socket for the desktop-pet / web UI:
//! a scout-connection toggle (`POST /api/control/scout`), a live SSE stream of Stage1 Interim
//! partials + Final utterances (`GET /api/stream`), and status (`GET /api/status`).
//!
//! Threading: the Pipeline runs Stage1 on a dedicated **std thread** (it blocks forever) and
//! Stage2 on its own internal `aura-stage2` worker (so partials never freeze behind a 1-2s LLM
//! route); the axum socket runs on a multi-thread tokio runtime on the main thread. The
//! Pipeline's `on_turn` callback (invoked from both pipeline threads) serializes each
//! [`TurnEvent`] to owned JSON and publishes it on a `broadcast::Sender<Value>` — the SSE
//! handler subscribes and streams it. Events carry their utterance `seq`; an interim for
//! utterance N+1 may arrive before the final of N (consumers group by seq, not arrival order).
//!
//! Run: cargo run -p aura-daemon --features asr,cuda -- 127.0.0.1:7879
//! Config precedence: CLI (high-frequency knobs, see `Cli`) > `aura.json` (full surface, dev:
//! this crate's dir, prod: ~/.desk-pilot/) > built-in defaults. No env vars.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{error, info, instrument, warn};
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

/// Streaming-ASR + Stage2 seed hotwords — the built-in default when `aura.json` doesn't set
/// `hotwords`. 真麦 #9 proved the mechanism end-to-end: in-list terms decode clean (Rust→RUST),
/// out-of-list ones shatter (Docker→DO CAR, GitHub→GUITAR, Kubernetes→KUBERNITIES). Seeded into
/// BOTH layers: baked into the streaming recognizer at boot, and preloading the shared store
/// Stage2 reads each turn. Stage3 grows the store at runtime (LLM layer only — pushing new words
/// down into the ASR recognizer is M5: needs a recognizer rebuild).
const SEED_HOTWORDS: &[&str] = &[
    "Rust", "Bevy", "Docker", "GitHub", "Kubernetes", "API", "Markdown", "PDF", "Agent",
    "README", "贪吃蛇", "蛇身", "计分器",
];

/// Runtime config (`CONF::aura.json` via the shared FileLoader — dev: this crate's dir;
/// prod: the unified `~/.desk-pilot/` folder). Every field is optional; precedence is
/// CLI arg / env var > config file > built-in default.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AuraConf {
    /// omni-scout `/audio` address (default `127.0.0.1:7878`).
    scout_addr: Option<String>,
    /// Daemon socket port (default 9091).
    port: Option<u16>,
    /// Run the in-process Stage3 rule trigger (default true).
    stage3: Option<bool>,
    /// Stage2 GGUF model file name, resolved inside the MODELS namespace.
    model: Option<String>,
    /// Seed hotwords for the streaming recognizer + the shared Stage2 store.
    hotwords: Option<Vec<String>>,
    /// Built SPA dist dir the daemon serves (default: workspace `dist/`).
    web_dist: Option<String>,
}

impl AuraConf {
    /// Load `CONF::aura.json`. A missing file is fine (all defaults); a malformed one is
    /// reported and ignored rather than killing the daemon.
    fn load() -> Self {
        let fs = shared::loader!();
        match fs.read_str("CONF::aura.json") {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(conf) => {
                    let from = fs
                        .resolve("CONF::aura.json")
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "aura.json".into());
                    info!(path = %from, "conf loaded");
                    conf
                }
                Err(e) => {
                    warn!(error = %e, "aura.json parse error — using defaults");
                    Self::default()
                }
            },
            Err(_) => {
                info!("no aura.json — using built-in defaults");
                Self::default()
            }
        }
    }
}

/// CLI — high-frequency knobs only; the FULL config surface lives in `aura.json`
/// (see [`AuraConf`]). Precedence: CLI > config file > built-in default.
#[derive(Debug, Default, Parser)]
#[command(
    name = "aura-daemon",
    about = "audio-aura daemon — Stage1→Stage2 voice pipeline + control socket",
    version
)]
struct Cli {
    /// omni-scout /audio address (e.g. 127.0.0.1:7878)
    scout_addr: Option<String>,
    /// Daemon socket port
    #[arg(short, long)]
    port: Option<u16>,
    /// Disable the in-process Stage3 rule trigger
    #[arg(long)]
    no_stage3: bool,
}

/// Fully-resolved runtime settings (what `main` actually runs on).
#[derive(Debug, PartialEq)]
struct Settings {
    scout_addr: String,
    port: u16,
    stage3_on: bool,
    model: String,
    hotwords: Vec<String>,
    web_dist: Option<String>,
}

/// Pure merge: CLI > `aura.json` > built-in default. (`--no-stage3` wins over the file;
/// model / hotwords / web_dist are config-file-only — low-frequency knobs.)
fn resolve(cli: Cli, conf: AuraConf) -> Settings {
    Settings {
        scout_addr: cli
            .scout_addr
            .or(conf.scout_addr)
            .unwrap_or_else(|| "127.0.0.1:7878".to_string()),
        port: cli.port.or(conf.port).unwrap_or(9091),
        stage3_on: !cli.no_stage3 && conf.stage3.unwrap_or(true),
        model: conf.model.unwrap_or_else(|| "Qwen3-1.7B-Q8_0.gguf".to_string()),
        hotwords: conf
            .hotwords
            .unwrap_or_else(|| SEED_HOTWORDS.iter().map(|s| s.to_string()).collect()),
        web_dist: conf.web_dist,
    }
}

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
    // Init-stage side effect, first thing in main: the process-wide tracing subscriber
    // (dev: human-readable; release: JSON lines; RUST_LOG filter, default info).
    shared::init_tracing();
    let s = resolve(Cli::parse(), AuraConf::load());
    let Settings { scout_addr, port, stage3_on, model, hotwords: seed_hotwords, web_dist } = s;

    // Connection toggle + event bus, shared across the Pipeline thread + socket handlers.
    let active = Arc::new(AtomicBool::new(true));
    let (bus, _rx) = broadcast::channel::<Value>(1024);

    // Shared hotword store = the Stage3→Stage2 feedback channel (seeded from the config /
    // built-in list; Stage3 grows it at runtime).
    let hotwords: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(seed_hotwords.clone()));
    let mgr: Arc<dyn HotwordManager> = Arc::new(SharedHotwordManager::new(Arc::clone(&hotwords)));
    let tool = AddHotwordTool::new(Arc::clone(&mgr));

    // Audio clip store for playback.
    let audio_clips: AudioClips = Arc::new(Mutex::new(HashMap::new()));

    info!("loading Stage1 (ONNX) + Stage2 (Qwen router) …");
    let mut cfg = Stage1Config::new(scout_addr.clone());
    cfg.active = Arc::clone(&active); // share the toggle with the executor
    // Bake the seed hotwords into the streaming recognizer (beam-search biasing).
    cfg.streaming.hotwords = seed_hotwords;
    let s1 = OnnxStage1Executor::new(cfg)?;
    let router = RouterEngine::load_default(&model)?;
    let _ = router.route_blocking("你好", None, &[]); // HF warmup
    let s2 = RouterStage2Calibrator::new(router, Arc::clone(&hotwords));

    // ── Pipeline on a dedicated std thread ── it bridges each TurnEvent to the event bus as
    //    owned JSON. Events carry their own utterance seq (assigned by Stage1).
    {
        let bus = bus.clone();
        let tool = tool.clone();
        let audio_clips = Arc::clone(&audio_clips);
        thread::Builder::new()
            .name("aura-pipeline".into())
            .spawn(move || {
                Pipeline::new(s1, s2).run(move |ev| {
                    match ev {
                        TurnEvent::Interim { seq, partial, at_s } => {
                            info!(seq, at_s, partial = %partial, "流式");
                            bus.send(json!({ "type":"interim", "seq":seq, "partial":partial, "at_s":at_s })).ok();
                        }
                        TurnEvent::Final { utterance: u, decision: d, route_ms } => {
                            // Log all three text layers — batch ASR (authoritative), streaming
                            // ASR (hotword-biased), and the Stage2 rewrite — so ASR-level loss is
                            // distinguishable from LLM rewriting when diagnosing "missing" words.
                            // (No pcm field: never log audio buffers.)
                            info!(
                                seq = u.seq,
                                at_s = u.at_s,
                                intent = %d.intent,
                                route_ms,
                                batch = %u.raw_text,
                                streaming = %u.streaming_text,
                                calibrated = %d.calibrated_text,
                                "final"
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
    info!(port, "socket: http://127.0.0.1:{port}  (/health /api/status /api/stream /api/control/scout /context)");
    info!(scout = %scout_addr, stage3 = stage3_on, "pipeline running on bg thread — Ctrl-C 结束");
    rt.block_on(serve_socket(state, port, web_dist));
    Ok(())
}

/// In-process Stage3 rule trigger (temporary; desktop-pet replaces it). Extracts uppercase-latin
/// proper-noun candidates from the calibrated text and adds them as hotwords — locking in Stage2's
/// corrections so future turns are reinforced. Concatenation artifacts ("APIdocker" — batch ASR
/// gluing adjacent terms) are rejected so they can't pollute the store.
#[instrument(skip(tool))]
fn stage3_rule_trigger(tool: &AddHotwordTool, text: &str) {
    for tok in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.len() < 2 || !tok.chars().any(|c| c.is_ascii_uppercase()) || looks_like_concat(tok)
        {
            continue;
        }
        if let Ok(out) = tool.invoke(&json!({ "word": tok })) {
            if out["added"].as_bool() == Some(true) {
                info!(word = %tok, "stage3 规则触发器加词");
            }
        }
    }
}

/// A concatenation artifact like "APIdocker": an UPPER-UPPER-lower trigram marks the glue seam
/// (the standard camelCase word-split rule). Legit tokens survive — "GitHub" (single-cap
/// boundaries), "README" (all caps, no lower after), "Rust" (TitleCase).
fn looks_like_concat(tok: &str) -> bool {
    let c: Vec<char> = tok.chars().collect();
    c.windows(3).any(|w| {
        w[0].is_ascii_uppercase() && w[1].is_ascii_uppercase() && w[2].is_ascii_lowercase()
    })
}

async fn serve_socket(state: DaemonState, port: u16, web_dist: Option<String>) {
    // Production: the daemon also serves the built SPA (same origin — no proxy needed). Resolve
    // dist/ from the workspace root (BASE minus "/native") so it's independent of the daemon's
    // cwd; override with `web_dist` (aura.json). In dev Vite serves the page (dist may be
    // absent → 404, harmless).
    let ws_root = BASE.strip_suffix("/native").unwrap_or(BASE);
    let dist_dir = web_dist.unwrap_or_else(|| format!("{ws_root}/dist"));
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

#[cfg(test)]
mod tests {
    use super::{looks_like_concat, resolve, AuraConf, Cli};

    #[test]
    fn concat_seam_rejected_legit_tokens_pass() {
        // Glue seams (UPPER-UPPER-lower trigram) — the "APIdocker" class.
        assert!(looks_like_concat("APIdocker"));
        assert!(looks_like_concat("PDFmarkdown"));
        assert!(looks_like_concat("APIs")); // plural junk, acceptable loss
        // Legit proper nouns survive.
        assert!(!looks_like_concat("Rust"));
        assert!(!looks_like_concat("GitHub")); // single-cap boundaries
        assert!(!looks_like_concat("README")); // all caps, no lower after
        assert!(!looks_like_concat("PDF"));
    }

    #[test]
    fn checked_in_aura_json_parses() {
        // Guard the dev runtime config against schema drift / typos.
        let s = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/aura.json"))
            .expect("apps/audio-aura/aura.json missing");
        let conf: AuraConf = serde_json::from_str(&s).expect("aura.json must parse");
        assert_eq!(conf.port, Some(9091));
        assert!(conf.hotwords.as_deref().is_some_and(|h| !h.is_empty()));
    }

    #[test]
    fn resolve_precedence_cli_over_conf_over_default() {
        // CLI wins over the file; file wins over defaults; --no-stage3 overrides stage3=true.
        let cli = Cli {
            scout_addr: Some("cli:1".into()),
            port: None,
            no_stage3: true,
        };
        let conf = AuraConf {
            scout_addr: Some("conf:2".into()),
            port: Some(1234),
            stage3: Some(true),
            model: None,
            hotwords: None,
            web_dist: Some("/tmp/dist".into()),
        };
        let s = resolve(cli, conf);
        assert_eq!(s.scout_addr, "cli:1");
        assert_eq!(s.port, 1234);
        assert!(!s.stage3_on, "--no-stage3 beats the config file");
        assert_eq!(s.model, "Qwen3-1.7B-Q8_0.gguf", "default model when unset");
        assert_eq!(s.hotwords.len(), super::SEED_HOTWORDS.len(), "seed fallback");
        assert_eq!(s.web_dist.as_deref(), Some("/tmp/dist"));

        // All-empty → pure defaults.
        let d = resolve(Cli::default(), AuraConf::default());
        assert_eq!(d.scout_addr, "127.0.0.1:7878");
        assert_eq!(d.port, 9091);
        assert!(d.stage3_on);
    }
}
