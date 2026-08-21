//! aura-daemon — the audio-aura binary entry point: config 解析 + tracing + 客户端 socket。
//! 流水线拼装(Stage1Config 组装/ASR·LLM 选型/模型加载/识别日志/窗口归档)全部在
//! aura-core 的 [`Pipeline::assemble`] —— 这里只产出 [`PipelineSpec`]、按下开关、搭服务。
//! Socket 面:
//! - `GET /api/state` — the complete [`AuraStateView`] snapshot (one source of truth).
//! - `GET /api/stream?state_changed_frequency=<ms>` — SSE: `hello`, then `state_changed` pings
//!   (throttled ≥250ms) whenever `version` advances. The client re-GETs /api/state on a ping.
//! - `POST /api/control/scout` (toggle), `POST /api/correct` (user correction), `GET /api/audio/:window_id`.
//!
//! Threading: the pipeline runs on a dedicated **std thread** ([`Pipeline::spawn`]) with Stage2
//! on its own internal `aura-stage2` worker (so partials never freeze behind a 1-2s LLM route);
//! the axum socket runs on a multi-thread tokio runtime on the main thread. The `on_turn`
//! callback pushes recognition [`AsrSegment`]s onto a broadcast channel
//! (the **data plane**, `/api/asr_stream`); settings changes (scout toggle / correction / Stage3
//! hotword) bump a global `version: AtomicU64` (the **control plane** — `/api/stream` pings
//! `state_changed`, clients re-GET `/api/state`). Recognition events do NOT bump `version`.
//!
//! Run: cargo run -p aura-daemon --features asr,cuda -- 127.0.0.1:7879
//! Config precedence: CLI (high-frequency knobs, see `Cli`) > `aura.yaml` (full surface, dev:
//! this crate's dir, prod: ~/.desk-pilot/) > built-in defaults. No env vars — except `RUST_LOG`,
//! which overrides the `log_level` setting as the standard tracing escape hatch.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{error, info, warn};
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use audio_aura_agent::{stage3_rule_trigger, AddHotwordTool, HotwordManager, SharedHotwordManager};
use audio_aura_core::archive::{ArchiveConfig, AudioArchive};
use audio_aura_core::hub::Storage;
use audio_aura_core::{AsrSpec, LlmInput, LlmSpec, Pipeline, PipelineSpec, StreamSpec, TurnEvent, VadSpec};

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

/// VAD / segmentation overrides from `aura.yaml`'s `asr.vad:` section (VAD 属 Stage1 语音
/// 前端,故挂 asr 下). All optional — an unset field falls back to the built-in default
/// (= `VadSpec::default`,与 Stage1Config 内置默认一致,core 有防漂移单测).
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct VadConf {
    /// Silero speech-probability threshold (default 0.5). Higher = less sensitive (fewer false
    /// triggers, may clip soft onsets); lower = more sensitive (may catch breath as speech).
    threshold: Option<f32>,
    /// Seconds of trailing silence to end a segment / fire EOS (default 1.0). Pauses shorter
    /// than this never split.
    min_silence: Option<f32>,
    /// Segments shorter than this are discarded by Silero's state machine (default 0.3).
    min_speech: Option<f32>,
    /// Force-split backstop for very long utterances, seconds (default 28.0).
    max_speech: Option<f32>,
    /// ★Merge-window gap, seconds (default 5.0) — the UPPER bound of the medium-interval
    /// window. Segments whose inter-speech silence < this join the SAME VadWindow (窗口级
    /// batch 重跑,权威文本); ≥ this settles the window → WindowEdge 定稿. Lower bound is
    /// implicit: `min_silence` is what splits segments in the first place, so the effective
    /// window is (min_silence, merge_gap) ≈ 1–2.5s. "什么算一句话"的旋钮。0 = 每段独立成窗。
    merge_gap: Option<f64>,
    /// ★Segment edge-extension, seconds (default 0.3; 0 = off). Silero cuts the soft onset
    /// (before its probability crosses `threshold`) and the fading coda (after it drops
    /// below) from every segment — the extension re-pads both edges from the recall buffer,
    /// so the batch ASR hears the real speech. Fixes "missing first/last character" on
    /// merged utterances.
    edge_margin: Option<f32>,
}

/// `asr.stream:` — 流式 ASR(恒本地,实时 partial 要低延迟)。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StreamConf {
    /// 流式引擎: "zipformer" (默认,当前唯一;未知值 assemble 报错)。
    model: Option<String>,
}

/// `asr:` — Stage1 语音前端:流式引擎 + 批式 ASR 部署选择 + VAD。`backend` 选边,
/// 未选中一侧的字段被忽略(写了也不生效)。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AsrConf {
    /// 流式 ASR(恒本地)。
    stream: StreamConf,
    /// Batch-ASR deployment: "local" (默认, in-process sherpa) | "remote" (HTTP) |
    /// "disable" (纯流式:不加载批式模型,batch_text 恒 None 回退流式文本)。
    backend: Option<String>,
    local: LocalAsrConf,
    remote: RemoteAsrConf,
    vad: Option<VadConf>,
}

/// `asr.local:` — in-process sherpa 批式 ASR。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LocalAsrConf {
    /// 批式引擎: "sensevoice" (默认) | "whisper" | "qwen3-asr"。
    model: Option<String>,
    /// ASR language code (default "auto")。
    language: Option<String>,
    /// onnxruntime 执行装置: "cpu" (默认) | "cuda"。GPU 只加速批式 —— VAD + 流式恒 CPU。
    /// cuda 需 GPU sherpa lib (cuDNN 9.25);CPU-only lib 下启动即失败。
    hardware: Option<String>,
    /// onnxruntime intra-op threads (default 8 = 8C/16T 甜点)。压低若它抢了流式线程。
    threads: Option<i32>,
    /// 模型根目录覆盖:所有模型路径(VAD/流式/批式)改在其下解析(默认 MODELS 命名空间)。
    model_dir: Option<String>,
}

/// `asr.remote:` — OpenAI 兼容 HTTP 批式 ASR。流式/VAD 仍本地。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RemoteAsrConf {
    /// 服务地址 (e.g. http://127.0.0.1:8000)。
    endpoint: Option<String>,
}

/// `llm:` — Stage2 LLM。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LlmConf {
    /// Deployment: "local" (默认, in-process mistral.rs) | "remote" (HTTP)。
    backend: Option<String>,
    /// local: GGUF 文件名(在 `model_dir` 或 MODELS 命名空间内);
    /// remote: 服务端模型名(传给 /v1/chat/completions)。
    model: Option<String>,
    /// local-only:模型根目录覆盖(默认 MODELS 命名空间)。
    model_dir: Option<String>,
    /// remote-only:服务地址。
    endpoint: Option<String>,
    /// Stage2 纠偏输入源: "batch" (默认) | "stream" | "both"。
    input: Option<LlmInput>,
}

/// `storage:` — 音频持久化。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StorageConf {
    /// Recordings base dir override (default: DATA::recordings — dev: this crate's data/,
    /// prod: ~/.desk-pilot/data/). Clips land in per-day subdirs (`<YYYY-MM-DD>/`).
    recordings_dir: Option<String>,
    /// 录音 + turn 日志保留期(天,默认 7)——短期记录供复盘/Stage3;超过该
    /// 天数的日期目录/文件被过期清理(启动时 + 每 24h)。
    retention_days: Option<u32>,
}

/// Runtime config (`CONF::aura.yaml` via the shared FileLoader — dev: this crate's dir;
/// prod: the unified `~/.desk-pilot/` folder; `aura.json` 向下兼容 fallback). Every field
/// is optional; precedence is CLI > config file > built-in default. **未知键直接拒绝**
/// (deny_unknown_fields):拼错/过时的键让 parse 失败 → Malformed 回退(warn + 内置默认),
/// 而不是静默不生效。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AuraConf {
    /// omni-scout `/audio` 地址 (default `127.0.0.1:7878`)。音频源,与 ASR 部署无关
    /// (local/remote 批式都吃它)。
    scout_addr: Option<String>,
    /// 客户端请求 scout 的推流 cadence (ms):`?chunk_ms=N` 让 scout 把源 buffer 聚合成
    /// N-ms 的 HTTP chunk 再推(不能快过 scout 的 node.quantum)。None = 不传,scout
    /// 按自身 quantum 速率推。纯网络层优化——消费侧照样重切 32ms 窗喂 VAD。
    scout_chunk_ms: Option<u64>,
    /// idle 深度睡眠(秒): 无客户端订阅 SSE 长连接持续此秒数 → Stage1 停止 + 断开 scout,
    /// 深度睡眠(CPU≈0); 下一个客户端连接时自动恢复。0/缺省 = 关闭。
    idle_timeout: Option<u64>,
    /// Daemon socket 监听地址 (default `127.0.0.1`)。
    bind_addr: Option<String>,
    /// Daemon socket port (default 9091)。
    port: Option<u16>,
    /// Run the in-process Stage3 rule trigger (default true)。
    stage3: Option<bool>,
    /// Log filter (default "info"). Accepts a plain level (trace|debug|info|warn|error) or
    /// full EnvFilter directives ("audio_aura_core=debug,info"). RUST_LOG still wins (escape
    /// hatch). High-frequency recognition logging (流式 partial / 纠偏碎片) is `debug` —
    /// set this to debug to watch the live pipeline.
    log_level: Option<String>,
    /// Built SPA dist dir the daemon serves (default: workspace `dist/`).
    web_dist: Option<String>,
    /// Seed hotwords for the streaming recognizer + the shared Stage2 store.
    hotwords: Option<Vec<String>>,
    asr: AsrConf,
    llm: LlmConf,
    storage: StorageConf,
}

/// Which config source [`AuraConf::load`] ended up on — reported by `main` AFTER tracing is
/// up (the load itself runs pre-subscriber, so it can't log for itself).
#[derive(Debug)]
enum ConfOrigin {
    Yaml,
    Json,
    /// Neither file found — all built-in defaults.
    Defaults,
    /// A file was found but didn't parse — carrying what went wrong (and which fallback won).
    Malformed(String),
}

impl AuraConf {
    /// Load `CONF::aura.yaml` (preferred — # comments, nesting-friendly), falling back to
    /// `CONF::aura.json`. A missing file is fine (all defaults); a malformed one is reported
    /// via the returned [`ConfOrigin`] (and skipped) rather than killing the daemon. Runs
    /// PRE-subscriber (main needs `log_level` before tracing init), so it returns its origin
    /// instead of logging.
    fn load() -> (Self, ConfOrigin) {
        let fs = shared::loader!();
        // 优先 aura.yaml (支持 # 注释, 嵌套友好); fallback aura.json (向下兼容).
        if let Ok(s) = fs.read_str("CONF::aura.yaml") {
            match serde_yaml::from_str::<Self>(&s) {
                Ok(conf) => return (conf, ConfOrigin::Yaml),
                Err(e) => {
                    let (conf, fallback) = Self::load_json_or_default(&fs);
                    return (
                        conf,
                        ConfOrigin::Malformed(format!("aura.yaml parse error: {e} — {fallback:?}")),
                    );
                }
            }
        }
        Self::load_json_or_default(&fs)
    }

    /// `aura.json` fallback — `Self::default()` when absent or unparsable. Returns which of
    /// the two won (for the [`ConfOrigin`] report).
    fn load_json_or_default(fs: &shared::FileLoader) -> (Self, ConfOrigin) {
        match fs.read_str("CONF::aura.json") {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(conf) => (conf, ConfOrigin::Json),
                Err(e) => (
                    Self::default(),
                    ConfOrigin::Malformed(format!("aura.json parse error: {e} — defaults")),
                ),
            },
            Err(_) => (Self::default(), ConfOrigin::Defaults),
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

/// Fully-resolved runtime settings (what `main` actually runs on). The pipeline subset is a
/// ready [`PipelineSpec`] — handed straight to [`Pipeline::assemble`].
#[derive(Debug, PartialEq)]
struct Settings {
    bind_addr: String,
    port: u16,
    stage3_on: bool,
    web_dist: Option<String>,
    recordings_dir: Option<String>,
    recordings_retention_days: u32,
    /// Log filter (RUST_LOG env still wins at subscriber init — see
    /// `shared::init_tracing_with_filter`).
    log_level: String,
    spec: PipelineSpec,
    /// idle 深度睡眠超时(秒); None = 关闭。
    idle_timeout: Option<u64>,
}

/// Pure merge: CLI > `aura.yaml` > built-in default. (`--no-stage3` wins over the file;
/// model / hotwords / web_dist / vad are config-file-only — low-frequency knobs.)
fn resolve(cli: Cli, conf: AuraConf) -> Settings {
    let AuraConf { scout_addr, scout_chunk_ms, idle_timeout, bind_addr, port, stage3, log_level, web_dist, hotwords, asr, llm, storage } = conf;
    // VAD: each field is the config value or the pipeline's built-in default
    // (`VadSpec::default` — pinned equal to Stage1Config's defaults by a core unit test,
    // so this can't drift from what the recognizer would use anyway).
    let v = asr.vad.unwrap_or_default();
    let d = VadSpec::default();
    let vad = VadSpec {
        threshold: v.threshold.unwrap_or(d.threshold),
        min_silence: v.min_silence.unwrap_or(d.min_silence),
        min_speech: v.min_speech.unwrap_or(d.min_speech),
        max_speech: v.max_speech.unwrap_or(d.max_speech),
        merge_gap: v.merge_gap.unwrap_or(d.merge_gap),
        edge_margin: v.edge_margin.unwrap_or(d.edge_margin),
    };
    // Stage1: 流式引擎(恒本地) + batch ASR (local / remote / disable)。
    let stream = StreamSpec {
        model: asr.stream.model.clone().unwrap_or_else(|| "zipformer".to_string()),
    };
    let asr_spec = match asr.backend.as_deref() {
        Some("remote") => AsrSpec::Remote { endpoint: asr.remote.endpoint.clone().unwrap_or_default() },
        Some("disable") => AsrSpec::Disabled,
        _ => AsrSpec::Local {
            backend: asr.local.model.clone().unwrap_or_else(|| "sensevoice".to_string()),
            language: asr.local.language.clone().unwrap_or_else(|| "auto".to_string()),
            hardware: asr.local.hardware.clone().unwrap_or_else(|| "cpu".to_string()),
            threads: asr.local.threads.unwrap_or(8),
            model_dir: asr.local.model_dir.clone(),
        },
    };
    // Stage2 LLM: local mistral.rs GGUF (可选 model_dir), remote OpenAI-compatible, 或禁用。
    let model = llm.model.clone().unwrap_or_else(|| "Qwen3-1.7B-Q8_0.gguf".to_string());
    let llm_spec = match llm.backend.as_deref() {
        Some("remote") => LlmSpec::Remote { endpoint: llm.endpoint.clone().unwrap_or_default(), model },
        Some("disable") => LlmSpec::Disabled,
        _ => LlmSpec::Local { model, model_dir: llm.model_dir.clone() },
    };
    Settings {
        bind_addr: bind_addr.unwrap_or_else(|| "127.0.0.1".to_string()),
        port: cli.port.or(port).unwrap_or(9091),
        stage3_on: !cli.no_stage3 && stage3.unwrap_or(true),
        web_dist,
        recordings_dir: storage.recordings_dir,
        recordings_retention_days: storage.retention_days.unwrap_or(7),
        log_level: log_level.unwrap_or_else(|| "info".to_string()),
        idle_timeout,
        spec: PipelineSpec {
            scout_addr: cli
                .scout_addr
                .or(scout_addr)
                .unwrap_or_else(|| "127.0.0.1:7878".to_string()),
            scout_chunk_ms,
            hotwords: hotwords
                .unwrap_or_else(|| SEED_HOTWORDS.iter().map(|s| s.to_string()).collect()),
            vad,
            stream,
            asr: asr_spec,
            llm: llm_spec,
            llm_input: llm.input.unwrap_or_default(),
        },
    }
}

// The AuraStateView snapshot + sub-types (ConfigView / VadView / CorrectionView / FinalView /
// UtteranceView) live in `audio_aura_agent::view` — the consumer-facing crate — shared with the
// `audio_aura_agent::client` SDK so server and Rust clients can't drift. The daemon constructs
// them; this module just imports.
use audio_aura_agent::{AsrSegment, AuraStateView, ConfigView, CorrectionView, VadView};

/// Shared daemon state surfaced over the socket.
#[derive(Clone)]
struct DaemonState {
    hotwords: Arc<Mutex<Vec<String>>>,
    corrections: Arc<Mutex<Vec<(String, String)>>>,
    /// Scout-connection toggle (shared with the Stage1 recognizer's ingest + run loop).
    active: Arc<AtomicBool>,
    /// idle 深度睡眠信号: false → Stage1 退出 + 断开 scout; 恢复时置回 true。
    running: Arc<AtomicBool>,
    /// 当前是否处于 idle 深度睡眠。
    idle: Arc<AtomicBool>,
    /// 活跃的 SSE 长连接订阅数(数据面 + 控制面)。idle 监控据此判断"无客户端"。
    subscribers: Arc<std::sync::atomic::AtomicUsize>,
    /// 恢复唤醒:pipeline 线程在 idle 后 park 在这里; 下一个客户端连接时 notify。
    resume_cv: Arc<(Mutex<()>, Condvar)>,
    /// idle 深度睡眠超时; None = 关闭。
    idle_timeout: Option<Duration>,
    /// Bumped on ANY SETTINGS change (connected / hotword / correction). Recognition events do
    /// NOT bump — they're pushed via `asr_events` (the data plane). The SSE handler ticks at the
    /// client's rate and pings only when this advances.
    version: Arc<AtomicU64>,
    /// Data-plane broadcast: recognition segments pushed directly to `GET /api/asr_stream`
    /// subscribers (low-latency, every event — unlike the throttled control-plane ping).
    asr_events: broadcast::Sender<AsrSegment>,
    config: ConfigView,
    stage3_on: bool,
    /// The Storage supervisor: audio archive (hot replay + date-named WAV flush) +
    /// per-turn day log + recent ring (backs /api/audio, /api/recordings).
    storage: Arc<Storage>,
}

impl DaemonState {
    /// Assemble the full [`AuraStateView`] snapshot — lock each source, clone, release. Called by
    /// GET /api/state (every change). No lock is held across an await (clones are synchronous).
    fn snapshot(&self) -> AuraStateView {
        let hotwords = self.hotwords.lock().unwrap().clone();
        let corrections = self
            .corrections
            .lock()
            .unwrap()
            .iter()
            .map(|(r, c)| CorrectionView { raw: r.clone(), corrected: c.clone() })
            .collect();
        AuraStateView {
            connected: self.active.load(Ordering::Relaxed),
            stage3_on: self.stage3_on,
            config: self.config.clone(),
            hotwords,
            corrections,
        }
    }

    /// Signal that state changed — the SSE handler's next eligible tick will ping clients.
    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Release);
    }

    /// 恢复识别:置 running=true + active=true(重连 scout), 唤醒 pipeline 线程重跑消费循环。
    fn resume(&self) {
        self.active.store(true, Ordering::Release);
        self.idle.store(false, Ordering::Release);
        if self.running.swap(true, Ordering::Release) == false {
            info!("client connected — resuming recognition");
        }
        self.resume_cv.1.notify_one();
    }

    /// 进入 idle 深度睡眠:running=false(Stage1 消费循环退出) + active=false(断开 scout)。
    fn enter_idle(&self) {
        if self.running.load(Ordering::Acquire) == true {
            info!("entering idle — no subscribers, disconnecting scout");
            self.running.store(false, Ordering::Release);
            self.active.store(false, Ordering::Release);
            self.idle.store(true, Ordering::Release);
        }
    }
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

fn main() -> Result<()> {
    // Config loads FIRST — tracing init needs the configured `log_level`. The load runs
    // pre-subscriber, so it returns its origin instead of logging; we report it right after
    // the subscriber is up (nothing is lost, just emitted post-init).
    let (conf, origin) = AuraConf::load();
    let s = resolve(Cli::parse(), conf);
    // Init-stage side effect: the process-wide tracing subscriber (dev: human-readable;
    // release: JSON lines). Filter precedence: RUST_LOG env (escape hatch) >
    // aura.yaml `log_level` > "info". `effective_level` reports what actually took effect
    // (differs from the configured value on RUST_LOG override or invalid-value fallback).
    let effective_level = shared::init_tracing_with_filter(&s.log_level);
    match &origin {
        ConfOrigin::Yaml => info!(log_level = %effective_level, "conf loaded (aura.yaml)"),
        ConfOrigin::Json => info!(log_level = %effective_level, "conf loaded (aura.json)"),
        ConfOrigin::Defaults => {
            info!(log_level = %effective_level, "no aura.yaml / aura.json — built-in defaults")
        }
        ConfOrigin::Malformed(what) => {
            warn!(what, log_level = %effective_level, "conf parse error — fallback in effect")
        }
    }
    let Settings { bind_addr, port, stage3_on, web_dist, recordings_dir, recordings_retention_days, log_level, spec, idle_timeout } = s;

    // Connection toggle + shared snapshot state, shared across the Pipeline thread + socket
    // handlers. (No event bus — SSE pings off the `version` counter; data lives in the snapshot.)
    let active = Arc::new(AtomicBool::new(true));
    // idle 深度睡眠信号: false → Stage1 消费循环退出 + 断开 scout; 恢复时置回 true。
    let running = Arc::new(AtomicBool::new(true));
    // idle 恢复唤醒: daemon 在下一个客户端连接时置 running=true + notify pipeline 线程。
    let resume_cv: Arc<(Mutex<()>, Condvar)> = Arc::new((Mutex::new(()), Condvar::new()));
    let version = Arc::new(AtomicU64::new(0));
    // Data-plane channel: recognition segments pushed to /api/asr_stream subscribers.
    let (asr_events, _) = broadcast::channel::<AsrSegment>(1024);
    let config = ConfigView {
        asr_backend: match &spec.asr {
            AsrSpec::Local { backend, .. } => backend.clone(),
            AsrSpec::Remote { .. } => "remote-http".to_string(),
            AsrSpec::Disabled => "streaming-only".to_string(),
        },
        asr_kind: spec.asr.kind().to_string(),
        asr_provider: match &spec.asr {
            AsrSpec::Local { hardware, .. } => hardware.clone(),
            AsrSpec::Remote { .. } | AsrSpec::Disabled => String::new(),
        },
        llm_kind: spec.llm.kind().to_string(),
        model: match &spec.llm {
            LlmSpec::Local { model, .. } | LlmSpec::Remote { model, .. } => model.clone(),
            LlmSpec::Disabled => String::new(),
        },
        vad: VadView {
            threshold: spec.vad.threshold,
            min_silence: spec.vad.min_silence,
            merge_gap: spec.vad.merge_gap,
        },
    };

    // Shared hotword store = the Stage3→Stage2 feedback channel (seeded from the config /
    // built-in list; Stage3 grows it at runtime).
    let hotwords: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(spec.hotwords.clone()));
    let corrections: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
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
    let retention_days = recordings_retention_days.max(1);
    info!(
        recordings = %rec_dir.display(),
        turns = %turns_dir.display(),
        retention_days,
        "storage ready (periodic flush + daily expired cleanup)"
    );
    let archive = Arc::new(AudioArchive::new(ArchiveConfig {
        dir: rec_dir,
        retention_days,
        ..Default::default()
    }));
    let storage = Arc::new(Storage::new(archive, turns_dir, retention_days));
    // 重启不丢历史:从磁盘重建索引 + 立即清理过期录音/日志。
    let cleaned = storage.init();
    if cleaned > 0 {
        info!(cleaned, "expired recordings/turn-logs cleaned at startup");
    }
    let _flusher = storage.audio.spawn_flusher();

    // ── 全栈拼装在 core(Stage1Config 组装/ASR·LLM 选型/模型加载/预热/识别日志/窗口归档)──
    // TODO: 这里是核心的模型推理触发点——assemble 加载模型,spawn 启动推理循环。
    let pipeline = Pipeline::assemble(
        &spec,
        Arc::clone(&active),
        Arc::clone(&running),
        Arc::clone(&hotwords),
        Arc::clone(&corrections),
        Some(Arc::clone(&storage)), // WindowCalibration 时自动 record_final(archive+day log+ring)
    )?;

    // ── Pipeline on its core-owned thread ── recognition segments → DATA plane; Stage3 on
    //    window finals. No event bus: the SSE handler pings off `version`.
    {
        let tool = tool.clone();
        let version = Arc::clone(&version);
        let asr_events = asr_events.clone();
        pipeline.spawn(Arc::clone(&running), Arc::clone(&resume_cv), move |ev| {
            // Recognition events → DATA plane only (broadcast the segment). The control
            // plane (version/snapshot) is NOT bumped here — only settings changes bump it.
            // (识别日志与窗口归档在 core 的 run() 内部——这里只做线协议映射。)
            let segment = match ev {
                TurnEvent::StreamFragment { window_id, segment_id, text, at_s } => {
                    Some(AsrSegment::StreamFragment {
                        window_id,
                        segment_id,
                        text: text.to_string(),
                        at_s,
                    })
                }
                TurnEvent::BatchSegment { window_id, segment_id, text } => {
                    Some(AsrSegment::BatchSegment { window_id, segment_id, text })
                }
                TurnEvent::BatchWindow { window_id, text } => {
                    Some(AsrSegment::BatchWindow { window_id, text })
                }
                TurnEvent::SegmentCalibration { window_id, calibrated, .. } => {
                    Some(AsrSegment::SegmentCalibration { window_id, calibrated })
                }
                TurnEvent::WindowCalibration { window_id, calibrated, route_ms } => {
                    // Stage3 may add hotwords — that's a SETTINGS change → control plane.
                    if stage3_on && stage3_rule_trigger(&tool, &calibrated) {
                        version.fetch_add(1, Ordering::Release);
                    }
                    let _ = route_ms;
                    Some(AsrSegment::WindowCalibration { window_id, calibrated })
                }
            };
            // Data plane: push the recognition segment directly to /api/asr_stream
            // subscribers (low-latency). Err only when there are no receivers (fine).
            if let Some(seg) = segment {
                let _ = asr_events.send(seg);
            }
        })?;
    }

    // ── Socket on the main thread's tokio runtime ──
    let state = DaemonState {
        hotwords: Arc::clone(&hotwords),
        corrections: Arc::clone(&corrections),
        active: Arc::clone(&active),
        running: Arc::clone(&running),
        idle: Arc::new(AtomicBool::new(false)),
        subscribers: Arc::new(AtomicUsize::new(0)),
        resume_cv: Arc::clone(&resume_cv),
        idle_timeout: idle_timeout.map(Duration::from_secs),
        version: Arc::clone(&version),
        asr_events: asr_events.clone(),
        config,
        stage3_on,
        storage,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("aura-socket")
        .build()?;
    // idle 深度睡眠监控:无 SSE 订阅持续 idle_timeout → 进入 idle(Stage1 退出 + 断开 scout)。
    if let Some(timeout) = state.idle_timeout {
        if timeout > Duration::ZERO {
            let mon = state.clone();
            rt.spawn(async move {
                let mut since: Option<Instant> = None;
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if mon.subscribers.load(Ordering::Relaxed) == 0 {
                        match since {
                            None => since = Some(Instant::now()),
                            Some(t) if t.elapsed() >= timeout => {
                                mon.enter_idle();
                                since = None;
                            }
                            Some(_) => {}
                        }
                    } else {
                        since = None;
                    }
                }
            });
        }
    }
    info!(port, "socket: http://{bind_addr}:{port}  (/api/state /api/stream /api/control/scout /api/correct /api/audio)");
    info!(scout = %spec.scout_addr, stage3 = stage3_on, log_level = %log_level, idle_timeout = ?idle_timeout, "pipeline running on bg thread — Ctrl-C 结束");
    rt.block_on(serve_socket(state, bind_addr, port, web_dist));
    Ok(())
}

async fn serve_socket(state: DaemonState, bind_addr: String, port: u16, web_dist: Option<String>) {
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
        // ── the snapshot-sync contract ──
        .route("/api/state", get(state_handler))           // full AuraStateView snapshot
        .route("/api/stream", get(stream_asr))             // control plane: hello → state_changed* (throttled)
        .route("/api/asr_stream", get(asr_stream))         // data plane: hello → recognition segments* (pushed)
        // ── actions (each mutates state → bumps version → next SSE tick pings) ──
        .route("/api/control/scout", post(control_scout))
        .route("/api/correct", post(correction_handler))
        // ── binary / queries ──
        .route("/api/audio/{seq}", get(audio_handler))
        .route("/api/recordings", get(recordings_handler))
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
async fn state_handler(State(s): State<DaemonState>) -> Json<AuraStateView> {
    Json(s.snapshot())
}

/// Toggle aura's OWN connection to scout (does NOT kill scout). Body: `{"enabled": bool}`.
async fn control_scout(State(s): State<DaemonState>, body: Json<Value>) -> Json<Value> {
    let enabled = body.get("enabled").and_then(|v| v.as_bool());
    let next = match enabled {
        Some(v) => v,
        None => !s.active.load(Ordering::Relaxed), // no arg → flip
    };
    s.active.store(next, Ordering::Relaxed);
    s.bump(); // connected changed → ping clients to re-fetch
    Json(json!({ "connected": next }))
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

/// `GET /api/asr_stream` — the DATA plane: pushes each recognition segment directly to the
/// subscriber (low-latency, every event — not throttled, unlike the control-plane `/api/stream`).
/// One `data: <AsrSegment json>\n\n` frame per recognition event (StreamFragment / BatchSegment
/// / BatchWindow / SegmentCalibration / WindowCalibration). Late/lagged subscribers get a
/// `lagged` comment (broadcast backlog overflowed) and keep going.
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

/// `GET /api/audio/:window_id` — serve the settled window's WAV for playback. The archive
/// resolves transparently: hot tier first, then the flushed file on disk.
async fn audio_handler(
    State(s): State<DaemonState>,
    Path(window_id): Path<u64>,
) -> impl IntoResponse {
    match s.storage.audio.wav(window_id) {
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

/// `POST /api/correct {window_id, raw, corrected}` — record a user correction for a settled
/// window: push to the Stage2 correction store, flag the timeline entry `corrected_by_user`,
/// and bump `version` so clients re-fetch and see the badge.
async fn correction_handler(State(s): State<DaemonState>, body: Json<Value>) -> Json<Value> {
    let raw = body.get("raw").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let corrected = body.get("corrected").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let window_id = body.get("window_id").and_then(|v| v.as_u64()).unwrap_or(0);
    if raw.is_empty() || corrected.is_empty() {
        return Json(json!({ "ok": false, "error": "raw and corrected required" }));
    }
    // Push to correction store (ring buffer, cap 20 — short-term memory for Stage2)
    {
        let mut c = s.corrections.lock().unwrap();
        if c.len() >= 20 {
            c.remove(0);
        } // evict oldest
        c.push((raw.clone(), corrected.clone()));
    }
    // Data plane: tell subscribers to mark the window corrected (the live list is client-side).
    let _ = s.asr_events.send(AsrSegment::Correction { window_id, raw, corrected });
    // Control plane: the corrections list changed → re-fetch snapshot.
    s.bump();
    info!("user correction added → Stage2");
    Json(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::{resolve, AuraConf, AsrConf, Cli, LlmConf, RemoteAsrConf};
    use audio_aura_core::{AsrSpec, LlmInput, LlmSpec};

    #[test]
    fn unknown_config_key_rejected() {
        // deny_unknown_fields:拼错/过时的键必须 parse 失败(→ Malformed warn + 默认),
        // 而不是静默不生效。旧平铺键(asr_backend 等)同理被拒 —— 一次性迁移可见。
        assert!(serde_yaml::from_str::<AuraConf>("asr_typo: 1").is_err());
        assert!(serde_yaml::from_str::<AuraConf>("asr_backend: sensevoice").is_err());
        assert!(serde_yaml::from_str::<AuraConf>("asr:\n  typo: 1").is_err());
        // 分层 + 已知键正常。
        assert!(serde_yaml::from_str::<AuraConf>("asr:\n  backend: remote").is_ok());
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
            web_dist: Some("/tmp/dist".into()),
            log_level: Some("debug".into()),
            ..Default::default()
        };
        let s = resolve(cli, conf);
        assert_eq!(s.spec.scout_addr, "cli:1");
        assert_eq!(s.port, 1234);
        assert_eq!(s.bind_addr, "127.0.0.1", "bind addr default");
        assert!(!s.stage3_on, "--no-stage3 beats the config file");
        assert!(matches!(&s.spec.llm, LlmSpec::Local { model, .. } if model == "Qwen3-1.7B-Q8_0.gguf"));
        assert_eq!(s.spec.hotwords.len(), super::SEED_HOTWORDS.len(), "seed fallback");
        assert_eq!(s.web_dist.as_deref(), Some("/tmp/dist"));
        assert_eq!(s.log_level, "debug", "log_level from the config file");

        // All-empty → pure defaults.
        let d = resolve(Cli::default(), AuraConf::default());
        assert_eq!(d.spec.scout_addr, "127.0.0.1:7878");
        assert_eq!(d.port, 9091);
        assert!(d.stage3_on);
        assert_eq!(d.log_level, "info", "log_level default");
        assert!(matches!(&d.spec.asr, AsrSpec::Local { backend, .. } if backend == "sensevoice"));
        // VAD defaults resolve to the built-ins (no vad: section ⇒ all-None ⇒ fallbacks).
        assert_eq!(d.spec.vad.merge_gap, 5.0);
        assert_eq!(d.spec.vad.threshold, 0.5);
        assert_eq!(d.spec.vad.min_silence, 1.0);
    }

    #[test]
    fn resolve_selects_remote_asr_llm() {
        // asr.backend / llm.backend = remote → 对应 Remote spec(endpoint 各自子节)。
        let conf = AuraConf {
            asr: AsrConf {
                backend: Some("remote".into()),
                remote: RemoteAsrConf { endpoint: Some("http://127.0.0.1:8000".into()) },
                ..Default::default()
            },
            llm: LlmConf {
                backend: Some("remote".into()),
                endpoint: Some("http://127.0.0.1:3000".into()),
                model: Some("m.gguf".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let s = resolve(Cli::default(), conf);
        assert!(matches!(&s.spec.asr, AsrSpec::Remote { endpoint } if endpoint == "http://127.0.0.1:8000"));
        assert!(matches!(&s.spec.llm, LlmSpec::Remote { endpoint, model }
            if endpoint == "http://127.0.0.1:3000" && model == "m.gguf"));
    }

    #[test]
    fn resolve_selects_disabled_batch() {
        // asr.backend: disable → 纯流式(不加载批式模型)。
        let conf = AuraConf {
            asr: AsrConf {
                backend: Some("disable".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let s = resolve(Cli::default(), conf);
        assert!(matches!(s.spec.asr, AsrSpec::Disabled));
        assert_eq!(s.spec.asr.kind(), "disabled");
        assert_eq!(s.spec.stream.model, "zipformer", "stream 默认引擎");
    }

    #[test]
    fn resolve_selects_disabled_llm() {
        // llm.backend: disable → Stage2 整体关闭(不加载 LLM,校准恒等)。
        let conf = AuraConf {
            llm: LlmConf {
                backend: Some("disable".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let s = resolve(Cli::default(), conf);
        assert!(matches!(s.spec.llm, LlmSpec::Disabled));
        assert_eq!(s.spec.llm.kind(), "disabled");
    }

    #[test]
    fn resolve_selects_llm_input() {
        // llm.input 默认 batch;显式配置 stream/both 时正确映射。
        let d = resolve(Cli::default(), AuraConf::default());
        assert_eq!(d.spec.llm_input, LlmInput::Batch, "默认 batch");

        let conf = AuraConf {
            llm: LlmConf {
                input: Some(LlmInput::Stream),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(resolve(Cli::default(), conf).spec.llm_input, LlmInput::Stream);

        let conf = AuraConf {
            llm: LlmConf {
                input: Some(LlmInput::Both),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(resolve(Cli::default(), conf).spec.llm_input, LlmInput::Both);
    }
}
