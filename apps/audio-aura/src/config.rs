//! 配置层—— CLI/yaml → [`Settings`] 的解析与合并(纯函数,无 I/O 副作用;文件
//! 读取在 [`AuraConf::load`],pre-subscriber 运行故只返回 origin 不打日志)。
//! 优先级:CLI(高频旋钮)> `aura.yaml`(全量面)> 内置默认。未知键直接拒绝
//! (deny_unknown_fields):拼错/过时的键 parse 失败 → Malformed 回退,而非静默失效。

use clap::Parser;
use serde::Deserialize;

use audio_aura_agent::{ConfigView, VadView};
use audio_aura_core::{AsrSpec, LlmInput, LlmSpec, PipelineSpec, StreamSpec, VadSpec};

/// Streaming-ASR + Stage2 seed hotwords — the built-in default when `aura.json` doesn't set
/// `hotwords`. 真麦 #9 proved the mechanism end-to-end: in-list terms decode clean (Rust→RUST),
/// out-of-list ones shatter (Docker→DO CAR, GitHub→GUITAR, Kubernetes→KUBERNITIES). Seeded into
/// BOTH layers: baked into the streaming recognizer at boot, and preloading the shared store
/// Stage2 reads each turn. Stage3 grows the store at runtime (LLM layer only — pushing new words
/// down into the ASR recognizer is M5: needs a recognizer rebuild).
pub(crate) const SEED_HOTWORDS: &[&str] = &[
    "Rust", "Bevy", "Docker", "GitHub", "Kubernetes", "API", "Markdown", "PDF", "Agent",
    "README", "贪吃蛇", "蛇身", "计分器",
];

/// VAD / sentence-segmentation overrides from `aura.yaml`'s `asr.vad:` section (VAD 属 Stage1 语音
/// 前端,故挂 asr 下). All optional — an unset field falls back to the built-in default
/// (= `VadSpec::default`,与 Stage1Config 内置默认一致,core 有防漂移单测).
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct VadConf {
    /// Silero speech-probability threshold (default 0.5). Higher = less sensitive (fewer false
    /// triggers, may clip soft onsets); lower = more sensitive (may catch breath as speech).
    threshold: Option<f32>,
    /// Seconds of trailing silence to end a sentence / fire EOS (default 1.0). Pauses shorter
    /// than this never split.
    min_silence: Option<f32>,
    /// Sentences shorter than this are discarded by Silero's state machine (default 0.3).
    min_speech: Option<f32>,
    /// Force-split backstop for very long utterances, seconds (default 28.0).
    max_speech: Option<f32>,
    /// ★Merge-paragraph gap, seconds (default 5.0) — the UPPER bound of the medium-interval
    /// paragraph. Sentences whose inter-speech silence < this join the SAME VadParagraph (段落级
    /// batch 重跑,权威文本); ≥ this settles the paragraph → ParagraphEdge 定稿. Lower bound is
    /// implicit: `min_silence` is what splits sentences in the first place, so the effective
    /// paragraph is (min_silence, merge_gap) ≈ 1–2.5s. "什么算一句话"的旋钮。0 = 每句独立成段。
    merge_gap: Option<f64>,
    /// ★Sentence edge-extension, seconds (default 0.3; 0 = off). Silero cuts the soft onset
    /// (before its probability crosses `threshold`) and the fading coda (after it drops
    /// below) from every sentence — the extension re-pads both edges from the recall buffer,
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
    /// 服务地址 (e.g. http://127.0.0.1:8080 — dp-router,统一 LLM + ASR)。
    endpoint: Option<String>,
    /// 服务端模型名(必填;OpenAI `/v1/audio/transcriptions` multipart form 必带 `model`)。
    /// 需与 dp-router.yaml `models[].name` 对齐(如 "qwen3-asr")。
    #[serde(default)]
    model: Option<String>,
}

/// `llm:` — Stage2 LLM。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LlmConf {
    /// Deployment: "remote" (默认,连 dp-router) | "disable" (Stage2 关闭)。
    backend: Option<String>,
    /// remote: 服务端模型名(传给 /v1/chat/completions)。默认 qwen2.5-3b-instruct-q4_k_m。
    model: Option<String>,
    /// remote: 服务地址(默认 http://127.0.0.1:8080,指向 dp-router)。
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
pub(crate) struct AuraConf {
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
pub(crate) enum ConfOrigin {
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
    pub(crate) fn load() -> (Self, ConfOrigin) {
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

/// CLI — high-frequency knobs only; the FULL config surface lives in `aura.yaml`
/// (see [`AuraConf`]). Precedence: CLI > config file > built-in default.
#[derive(Debug, Default, Parser)]
#[command(
    name = "aura-daemon",
    about = "audio-aura daemon — Stage1→Stage2 voice pipeline + control socket",
    version
)]
pub(crate) struct Cli {
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
pub(crate) struct Settings {
    pub(crate) bind_addr: String,
    pub(crate) port: u16,
    pub(crate) stage3_on: bool,
    pub(crate) web_dist: Option<String>,
    pub(crate) recordings_dir: Option<String>,
    pub(crate) recordings_retention_days: u32,
    /// Log filter (RUST_LOG env still wins at subscriber init — see
    /// `shared::init_tracing_with_filter`).
    pub(crate) log_level: String,
    pub(crate) spec: PipelineSpec,
    /// idle 深度睡眠超时(秒); None = 关闭。
    pub(crate) idle_timeout: Option<u64>,
}

/// Pure merge: CLI > `aura.yaml` > built-in default. (`--no-stage3` wins over the file;
/// model / hotwords / web_dist / vad are config-file-only — low-frequency knobs.)
pub(crate) fn resolve(cli: Cli, conf: AuraConf) -> Settings {
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
        Some("remote") => AsrSpec::Remote {
                endpoint: asr.remote.endpoint.clone().unwrap_or_default(),
                model: asr.remote.model.clone().unwrap_or_else(|| "qwen3-asr".to_string()),
            },
        Some("disable") => AsrSpec::Disabled,
        _ => AsrSpec::Local {
            backend: asr.local.model.clone().unwrap_or_else(|| "sensevoice".to_string()),
            language: asr.local.language.clone().unwrap_or_else(|| "auto".to_string()),
            hardware: asr.local.hardware.clone().unwrap_or_else(|| "cpu".to_string()),
            threads: asr.local.threads.unwrap_or(8),
            model_dir: asr.local.model_dir.clone(),
        },
    };
    // Stage2 LLM: remote OpenAI-compatible (默认连 dp-router :8080) 或 disable。
    let model = llm.model.clone().unwrap_or_else(|| "qwen2.5-3b-instruct-q4_k_m".to_string());
    let endpoint = llm.endpoint.clone().unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let llm_spec = match llm.backend.as_deref() {
        Some("disable") => LlmSpec::Disabled,
        // 缺省 / 显式 remote → Remote(包含 "remote" 外的任何 backend 字符串:宽容兼旧配置)。
        _ => LlmSpec::Remote { endpoint, model },
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

/// 从选型结果构造 `/api/state` 快照里的只读配置视图(ConfigView)。
pub(crate) fn config_view(spec: &PipelineSpec) -> ConfigView {
    ConfigView {
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
            LlmSpec::Remote { model, .. } => model.clone(),
            LlmSpec::Disabled => String::new(),
        },
        vad: VadView {
            threshold: spec.vad.threshold,
            min_silence: spec.vad.min_silence,
            merge_gap: spec.vad.merge_gap,
        },
    }
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
        assert!(matches!(&s.spec.llm, LlmSpec::Remote { model, endpoint }
            if model == "qwen2.5-3b-instruct-q4_k_m" && endpoint == "http://127.0.0.1:8080"));
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
                remote: RemoteAsrConf { endpoint: Some("http://127.0.0.1:8080".into()), model: None },
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
        assert!(matches!(&s.spec.asr, AsrSpec::Remote { endpoint, .. } if endpoint == "http://127.0.0.1:8080"),
            "endpoint = 测试数据里给的 8080(原断言写 8000 是笔误)");
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
        assert!(matches!(&s.spec.asr, AsrSpec::Disabled));
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
        assert!(matches!(&s.spec.llm, LlmSpec::Disabled));
        assert_eq!(s.spec.llm.kind(), "disabled");
    }

    #[test]
    fn resolve_selects_llm_input() {
        // llm.input 默认 batch;显式配置 stream/both 时正确映射。
        let d = resolve(Cli::default(), AuraConf::default());
        assert_eq!(d.spec.llm_input, LlmInput::Both, "默认 both(双通道:batch+流式都传)");

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
