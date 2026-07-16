//! audio-aura-core — the audio-aura Rust daemon (M-transport spine).
//! Loads the GPU RouterEngine once, opens the shared SQLite store, runs an axum server exposing the
//! same API surface as the TS backend (SSE event stream + REST). Devtools-web connects here.

mod audio;
#[cfg(feature = "asr")]
mod ingest;
mod pipeline;
mod routes;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast;
use audio_aura_router::RouterEngine;
use audio_aura_store::Store;

/// Remote LLM config for the writer fallback (Anthropic-compatible proxy).
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub pro_model: String,
}

/// Shared daemon state (single-user: one store, one router, one event bus).
pub struct AppState {
    pub store: Arc<Store>,
    pub bus: broadcast::Sender<serde_json::Value>,
    pub router: Arc<RouterEngine>,
    pub audio_dir: String,
    pub llm: LlmConfig,
}

impl AppState {
    /// Fan out one JSON event to all connected display clients (SSE).
    pub fn emit(&self, ev: serde_json::Value) {
        let _ = self.bus.send(ev);
    }
}

/// Strip a trailing `[..]` harness suffix (e.g. `deepseek-v4-pro[1m]`) that upstream rejects.
fn clean_model(s: Option<String>, default: &str) -> String {
    let raw = s.unwrap_or_default();
    let raw = raw.trim();
    let cleaned = match raw.rfind('[') {
        Some(idx) if raw.ends_with(']') => raw[..idx].trim(),
        _ => raw,
    };
    if cleaned.is_empty() {
        default.to_string()
    } else {
        cleaned.to_string()
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() -> Result<()> {
    let host = env_or("HOST", "127.0.0.1");
    let port: u16 = std::env::var("PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(9090);
    let db_path = env_or("VOICE_DB_PATH", "./data/voice-agent.db");
    let audio_dir = env_or("VOICE_AUDIO_DIR", "./data/audio");
    let model_dir = env_or("VOICE_MODEL_DIR", "./native/models");
    let model_file = env_or("VOICE_MODEL_FILE", "Qwen3-1.7B-Q8_0.gguf");

    let llm = LlmConfig {
        base_url: env_or("ANTHROPIC_BASE_URL", "https://api.deepseek.com/anthropic")
            .trim_end_matches('/')
            .to_string(),
        api_key: std::env::var("ANTHROPIC_AUTH_TOKEN")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .unwrap_or_default(),
        pro_model: clean_model(
            std::env::var("VOICE_MODEL_PRO").ok().or_else(|| std::env::var("ANTHROPIC_MODEL").ok()),
            "deepseek-v4-pro",
        ),
    };

    let store = Arc::new(Store::open(&db_path)?);
    // Load the model BEFORE entering a tokio runtime — RouterEngine::load builds its own runtime and
    // block_on's; doing this inside an async context would panic (runtime-within-runtime).
    eprintln!("[audio-aura-core] loading router {model_file} (GPU if built with --features cuda) …");
    let router = Arc::new(RouterEngine::load(&model_dir, &model_file)?);
    eprintln!("[audio-aura-core] router ready");

    let (tx, _rx) = broadcast::channel::<serde_json::Value>(1024);
    let state = Arc::new(AppState { store, bus: tx, router, audio_dir, llm });

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let app = routes::router(state.clone());
        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        println!("\n  语音秘书 Rust 守护进程 (audio-aura-core) — http://{addr}");
        println!("  writer LLM: {}  pro={}", state.llm.base_url, state.llm.pro_model);
        println!("  DB: {db_path}   audio: {}", state.audio_dir);
        println!("  GET /api/health · GET /api/stream(SSE) · POST /api/turn · /api/dev/inject-turn · /api/topics*\n");
        // Stage1: start the audio ingest (omni-scout /audio → VAD+ASR → pipeline) if configured.
        #[cfg(feature = "asr")]
        if let Ok(scout) = std::env::var("SCOUT_AUDIO_URL") {
            let model = env_or("VOICE_ASR_MODEL", "./native/models/sensevoice/model.int8.onnx");
            let tokens = env_or("VOICE_ASR_TOKENS", "./native/models/sensevoice/tokens.txt");
            let st = state.clone();
            let h = tokio::runtime::Handle::current();
            let scout_log = scout.clone();
            std::thread::spawn(move || {
                if let Err(e) = ingest::run_ingest(st, h, scout, model, tokens) {
                    eprintln!("[ingest] fatal: {e}");
                }
            });
            println!("  Stage1 ASR ingest: SenseVoice ← omni-scout {scout_log}/audio");
        }

        axum::serve(listener, app).await?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}
