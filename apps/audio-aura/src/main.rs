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
use audio_aura_store::archive::{ArchiveConfig, AudioArchive};
use audio_aura_store::hub::{FinalTurn, Storage};
use audio_aura_asr::executor::{OnnxStage1Executor, Stage1Config};
use audio_aura_core::{Pipeline, TurnEvent};
use audio_aura_router::calibrator::Stage2CalibratorImpl;
use audio_aura_router::Calibrator;

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
    /// Stage1 batch ASR backend: "sensevoice" (default) | "whisper" | "qwen3-asr".
    asr_backend: Option<String>,
    /// ASR language code (default "auto" for SenseVoice, "zh" for Whisper).
    asr_language: Option<String>,
    /// Batch-ASR ONNX provider: "cpu" (default) | "cuda". GPU only helps the BATCH ASR
    /// (SenseVoice/Whisper/Qwen3) — VAD + streaming stay CPU regardless. Requires the
    /// CUDA-enabled sherpa shared lib; with the CPU-only lib, "cuda" fails at startup.
    asr_provider: Option<String>,
    /// Batch-ASR onnxruntime intra-op threads (default 8 = sweet spot on 8C/16T; 2 wastes
    /// cores, 16 contends on mem bandwidth). Lower if it starves the streaming recognizer.
    asr_threads: Option<i32>,
    /// Seed hotwords for the streaming recognizer + the shared Stage2 store.
    hotwords: Option<Vec<String>>,
    /// Built SPA dist dir the daemon serves (default: workspace `dist/`).
    web_dist: Option<String>,
    /// Recordings base dir override (default: DATA::recordings — dev: this crate's data/,
    /// prod: ~/.desk-pilot/data/). Clips land in per-day subdirs (`<YYYY-MM-DD>/`).
    recordings_dir: Option<String>,
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
    asr_backend: String,
    asr_language: String,
    asr_provider: String,
    asr_threads: i32,
    hotwords: Vec<String>,
    web_dist: Option<String>,
    recordings_dir: Option<String>,
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
        asr_backend: conf.asr_backend.unwrap_or_else(|| "sensevoice".to_string()),
        asr_language: conf.asr_language.unwrap_or_else(|| "auto".to_string()),
        asr_provider: conf.asr_provider.unwrap_or_else(|| "cpu".to_string()),
        asr_threads: conf.asr_threads.unwrap_or(8),
        hotwords: conf
            .hotwords
            .unwrap_or_else(|| SEED_HOTWORDS.iter().map(|s| s.to_string()).collect()),
        web_dist: conf.web_dist,
        recordings_dir: conf.recordings_dir,
    }
}

/// Shared daemon state surfaced over the socket.
#[derive(Clone)]
struct DaemonState {
    hotwords: Arc<Mutex<Vec<String>>>,
    /// Scout-connection toggle (shared with Stage1Executor's ingest + run loop).
    active: Arc<AtomicBool>,
    /// Event bus bridging the Pipeline thread → SSE clients.
    bus: broadcast::Sender<Value>,
    /// The Storage supervisor: audio archive (hot replay + date-named WAV flush) +
    /// per-turn day log + recent ring (backs /api/audio, /api/recordings, /results).
    storage: Arc<Storage>,
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
    let Settings { scout_addr, port, stage3_on, model, asr_backend, asr_language, asr_provider, asr_threads, hotwords: seed_hotwords, web_dist, recordings_dir } = s;

    // Connection toggle + event bus, shared across the Pipeline thread + socket handlers.
    let active = Arc::new(AtomicBool::new(true));
    let (bus, _rx) = broadcast::channel::<Value>(1024);

    // Shared hotword store = the Stage3→Stage2 feedback channel (seeded from the config /
    // built-in list; Stage3 grows it at runtime).
    let hotwords: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(seed_hotwords.clone()));
    let mgr: Arc<dyn HotwordManager> = Arc::new(SharedHotwordManager::new(Arc::clone(&hotwords)));
    let tool = AddHotwordTool::new(Arc::clone(&mgr));

    // Storage supervisor: audio archive (date-named WAVs under recordings/<YYYY-MM-DD>/) +
    // per-turn day log (turns/<YYYY-MM-DD>.jsonl) + recent ring. Dirs: aura.json
    // `recordings_dir` override, else DATA:: (dev: apps/audio-aura/data/, prod: ~/.desk-pilot/data/).
    let data = shared::loader!();
    let rec_dir = recordings_dir.map(std::path::PathBuf::from).unwrap_or_else(|| {
        data.resolve("DATA::recordings")
            .unwrap_or_else(|| std::path::PathBuf::from("data/recordings"))
    });
    let turns_dir = data
        .resolve("DATA::turns")
        .unwrap_or_else(|| std::path::PathBuf::from("data/turns"));
    info!(recordings = %rec_dir.display(), turns = %turns_dir.display(), "storage ready (periodic flush)");
    let archive = Arc::new(AudioArchive::new(ArchiveConfig { dir: rec_dir, ..Default::default() }));
    let _flusher = archive.spawn_flusher();
    let storage = Arc::new(Storage::new(archive, turns_dir));

    info!("loading Stage1 (ONNX) + Stage2 (Qwen calibrator) …");
    let mut cfg = Stage1Config::new(scout_addr.clone());
    cfg.active = Arc::clone(&active); // share the toggle with the executor
    // Bake the seed hotwords into the streaming recognizer (beam-search biasing).
    cfg.streaming.hotwords = seed_hotwords;
    // Select batch ASR backend from config (default: SenseVoice).
    //   "whisper"   → large-v3-turbo
    //   "qwen3-asr" → Qwen3-Audio ASR 1.7B int8 (high accuracy, slow on CPU)
    if asr_backend == "whisper" {
        info!("ASR backend: Whisper large-v3-turbo (language: {asr_language})");
        cfg = cfg.with_whisper_asr(&asr_language);
    } else if asr_backend == "qwen3-asr" {
        info!("ASR backend: Qwen3-Audio ASR 1.7B int8 (CPU-only build ⇒ slow per utterance)");
        cfg = cfg.with_qwen3_asr();
    } else {
        info!("ASR backend: SenseVoice (language: {asr_language})");
    }
    // Batch-ASR ONNX provider (VAD + streaming stay CPU). "cuda" needs the GPU sherpa lib +
    // cuDNN 9.25+ (native/cudnn) for correct sm_120 (Blackwell) numerics. SenseVoice+cuda is the
    // fast path; Qwen3-ASR+cuda is now correct too (cuDNN 9.25 fixed the old 9.1 mis-decode) but
    // its autoregressive decoder isn't faster than CPU.
    cfg.asr.provider = asr_provider.clone();
    cfg.asr.num_threads = asr_threads;
    if asr_backend == "qwen3-asr" && asr_provider == "cuda" {
        info!("Qwen3-ASR on CUDA: correct (cuDNN 9.25) but autoregressive ⇒ ~CPU speed; Qwen3 is fastest on CPU");
    }
    info!("ASR provider: {} | threads: {} (batch ASR; VAD + streaming on CPU)", cfg.asr.provider, cfg.asr.num_threads);
    let s1 = OnnxStage1Executor::new(cfg)?;
    let calibrator = Calibrator::load_default(&model)?;
    let _ = calibrator.calibrate_blocking("你好", None, &[]); // HF warmup
    let s2: Stage2CalibratorImpl = Stage2CalibratorImpl::new(calibrator, Arc::clone(&hotwords));

    // ── Pipeline on a dedicated std thread ── it bridges each TurnEvent to the event bus as
    //    owned JSON. Events carry their own utterance seq (assigned by Stage1).
    {
        let bus = bus.clone();
        let tool = tool.clone();
        let storage = Arc::clone(&storage);
        thread::Builder::new()
            .name("aura-pipeline".into())
            .spawn(move || {
                // TODO: 这里是核心的模型推理触发点；
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
                            // One call records everywhere: PCM → audio archive,
                            // transcript+decision → day log + recent ring (/results).
                            storage.record_final(FinalTurn {
                                seq: u.seq,
                                at_s: u.at_s,
                                duration_ms: u.duration_ms,
                                raw_text: u.raw_text.clone(),
                                streaming_text: u.streaming_text.clone(),
                                calibrated: d.calibrated_text.clone(),
                                intent: d.intent.clone(),
                                reply: d.reply.clone(),
                                route_ms,
                                pcm: u.pcm.clone(),
                            });
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
    let state = DaemonState { hotwords: Arc::clone(&hotwords), active: Arc::clone(&active), bus, storage };
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
    // TODO: 硬编码了 static 文件路径，使用 FileLoader 提供的机制来处理
    let ws_root = BASE.strip_suffix("/native").unwrap_or(BASE);
    let dist_dir = web_dist.unwrap_or_else(|| format!("{ws_root}/dist"));
    let static_spa = ServeDir::new(&dist_dir).fallback(ServeFile::new(format!("{dist_dir}/index.html")));
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/status", get(status_handler))
        .route("/api/control/scout", post(control_scout))
        .route("/api/stream", get(stream_asr))
        .route("/api/audio/{seq}", get(audio_handler))
        .route("/api/recordings", get(recordings_handler))
        .route("/context", get(context_handler))
        // remaining stubs (speaker / results / annotate) — out of scope this round
        .route("/control/speaker", post(control_stub))
        .route("/results", get(results_handler))
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

/// `GET /api/audio/:seq` — serve utterance `seq` as a WAV for playback. The archive resolves
/// transparently: hot tier first, then the flushed file on disk.
async fn audio_handler(
    State(s): State<DaemonState>,
    Path(seq): Path<u64>,
) -> impl IntoResponse {
    match s.storage.audio.wav(seq) {
        Some(wav) => {
            ([(axum::http::header::CONTENT_TYPE, "audio/wav")], wav).into_response()
        }
        None => (axum::http::StatusCode::NOT_FOUND, "audio clip not found").into_response(),
    }
}

/// `GET /api/recordings` — list all known clips (hot + flushed), ascending seq.
async fn recordings_handler(State(s): State<DaemonState>) -> Json<Value> {
    Json(json!({ "recordings": s.storage.recordings() }))
}

async fn control_stub() -> Json<Value> {
    Json(json!({ "stub": true, "todo": "speaker control + runtime config" }))
}
/// `GET /results` — recent Stage1+Stage2 turn records (oldest → newest, bounded ring).
async fn results_handler(State(s): State<DaemonState>) -> Json<Value> {
    Json(json!({ "results": s.storage.recent() }))
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
            recordings_dir: None,
            ..Default::default()
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
