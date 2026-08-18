//! pipeline (原 composer) — the `Pipeline` (组装车间): wires [`Stage1Recognizer`] →
//! [`Stage2Calibrator`] and emits [`TurnEvent`]s to a caller-supplied callback. Pure
//! orchestration — it does no printing, no file I/O, no Stage3 logic.
//!
//! **拼装也在这里**: [`PipelineSpec`] 是"选什么模型/什么参数"的完整描述（daemon 的
//! resolve() 产出），[`Pipeline::assemble`] 把它变成可运行的 Pipeline —— Stage1Config
//! 逐字段落位 + ASR 后端选择 + Stage2 LLM 选择（local mistral.rs / remote HTTP）+ 模型
//! 加载与预热。daemon 只负责 config 解析、socket 和 Stage3 触发；识别事件日志
//! （流式/纠偏）与窗口归档（[`Storage::record_final`]）也在 run() 内部，不劳调用方。
//!
//! Stage2 calibration runs on its own `aura-stage2` worker thread so the Stage1 consume loop
//! never blocks on the LLM — streaming partials keep flowing while a window is being
//! calibrated. An `Interim` for segment N+1 can arrive BEFORE the `WindowFinal` for window N.
//!
//! The worker drains the two Stage1 triggers off an mpsc channel (Interim never crosses the
//! channel — it passes straight through on the Stage1 thread): `Batch` →
//! [`Stage2Calibrator::calibrate_window`] (joint calibration of the current window, result
//! overwrites the window's stored calibration) → [`TurnEvent::WindowCalibrated`];
//! `WindowEdge` → [`Stage2Calibrator::finalize_window`] (NO LLM — attach the stored
//! calibration as the window's final field, move the left boundary) →
//! [`TurnEvent::WindowFinal`].

use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::{bail, Result};
use tracing::{debug, info};

use crate::calibrator::{Stage2Calibrator, Stage2CalibratorImpl};
use crate::hub::{FinalTurn, Storage};
use crate::recognizer::{OnnxStage1Recognizer, Stage1Config, Stage1Recognizer};
use crate::{Calibrator, Stage1Event};

use dp_models::http::HttpLlm;

// ── PipelineSpec — 选型描述（daemon resolve() 产出，assemble() 消费）────────────────
// 分层:daemon 负责"从哪儿读配置"(yaml/json/CLI/默认值),这里只认 fully-resolved 的
// 具体值 —— 线协议/文件格式不进 core。VadSpec::default 与 Stage1Config::new 的内置
// 默认一致(单测钉死,防两处漂移)。

/// Fully-resolved pipeline 选型:音频源、种子热词、VAD/分段参数、流式 ASR、Stage1 batch
/// ASR、Stage2 LLM。[`Pipeline::assemble`] 的唯一输入(运行时共享句柄除外)。
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSpec {
    /// omni-scout `/audio` 地址。
    pub scout_addr: String,
    /// 种子热词:烘烤进流式 recognizer(beam bias),并预载 Stage2 共享 store。
    pub hotwords: Vec<String>,
    pub vad: VadSpec,
    pub stream: StreamSpec,
    pub asr: AsrSpec,
    pub llm: LlmSpec,
}

/// 流式 ASR 选型(**恒本地** —— 实时 partial 要低延迟,不走 remote)。当前唯一引擎
/// zipformer;新引擎落地时在 [`stage1_config`] 的 match 里扩臂。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSpec {
    /// "zipformer" (当前唯一;未知值 assemble 直接报错)。
    pub model: String,
}

/// VAD/分段参数(具体值)。[`Default`] 与 [`Stage1Config::new`] 的内置默认逐字段一致。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadSpec {
    /// Silero speech-probability threshold(0.5)。高=不敏感,低=易误触。
    pub threshold: f32,
    /// 切段间隔秒(1.0)——短于此的停顿不切段。
    pub min_silence: f32,
    /// 短于该时长的段被 Silero 丢弃(0.3)。
    pub min_speech: f32,
    /// 超长强切兜底秒(28.0)。
    pub max_speech: f32,
    /// ★merge 窗口间隔秒(5.0)——"什么算一句话"的上界;0 = 每段独立成窗。
    pub merge_gap: f64,
    /// 段边界扩展秒(0.3;0=off)——补 Silero 切掉的软起音/尾音。
    pub edge_margin: f32,
}

impl Default for VadSpec {
    fn default() -> Self {
        VadSpec {
            threshold: 0.5,
            min_silence: 1.0,
            min_speech: 0.3,
            max_speech: 28.0,
            merge_gap: 5.0,
            edge_margin: 0.3,
        }
    }
}

/// Stage1 batch ASR 选型。流式 ASR + VAD 恒为本地 sherpa(实时 partial 要低延迟),
/// 这里只选 batch 通道。
#[derive(Debug, Clone, PartialEq)]
pub enum AsrSpec {
    /// 本地 ONNX:backend "sensevoice"(默认) | "whisper" | "qwen3-asr";
    /// hardware "cpu"(默认) | "cuda"(仅 batch;cuDNN 9.25+);threads = intra-op 并行;
    /// model_dir = 模型根目录覆盖(None → MODELS 命名空间,含流式/VAD 路径)。
    Local {
        backend: String,
        language: String,
        hardware: String,
        threads: i32,
        model_dir: Option<String>,
    },
    /// 远程 HTTP(OpenAI 兼容 `/v1/audio/transcriptions`)。流式/VAD 仍走 MODELS 命名空间。
    Remote { endpoint: String },
    /// 批式整体禁用(纯流式模式):不加载批式模型,`batch_text` 恒 `None` —— 消费方
    /// 按设计回退流式文本。省掉段级/窗口级 batch 调用(远程 ~3.5s/次)。
    Disabled,
}

impl AsrSpec {
    /// "local" | "remote" | "disabled" — 配置快照(ConfigView)的显示标签。
    pub fn kind(&self) -> &'static str {
        match self {
            AsrSpec::Local { .. } => "local",
            AsrSpec::Remote { .. } => "remote",
            AsrSpec::Disabled => "disabled",
        }
    }
}

/// Stage2 LLM 选型。
#[derive(Debug, Clone, PartialEq)]
pub enum LlmSpec {
    /// 本地 mistral.rs(Candle GGUF)。`model` = GGUF 文件名;`model_dir` = 根目录覆盖
    /// (None → MODELS 命名空间)。
    Local { model: String, model_dir: Option<String> },
    /// 远程 HTTP(OpenAI 兼容 `/v1/chat/completions`,vLLM/SGLang)。`model` = 服务端模型名。
    Remote { endpoint: String, model: String },
}

impl LlmSpec {
    /// "local" | "remote" — 配置快照(ConfigView)的显示标签。
    pub fn kind(&self) -> &'static str {
        match self {
            LlmSpec::Local { .. } => "local",
            LlmSpec::Remote { .. } => "remote",
        }
    }
}

/// One turn surfaced to the caller.
#[derive(Debug)]
pub enum TurnEvent<'a> {
    /// Live streaming partial for the CURRENT segment (raw, uncalibrated). Straight from the
    /// Stage1 thread — NOT a Stage2 input (D2: no live-partial calibration).
    Interim { window_id: u64, segment_id: u64, partial: &'a str, at_s: f64 },
    /// Stage2's provisional JOINT calibration of the current window (per Batch) — the
    /// calibrated text so far, replacing the previous calibration of the same window.
    WindowCalibrated { window_id: u64, calibrated: String, route_ms: f64 },
    /// The settled window's final calibration (per WindowEdge) — the window's LAST joint
    /// calibration attached as its field (no extra LLM run). Window-granularity final (D3).
    WindowFinal { window: &'a crate::VadWindow, calibrated: String, route_ms: f64 },
}

pub struct Pipeline {
    s1: OnnxStage1Recognizer,
    s2: Box<dyn Stage2Calibrator>,
    /// Some → run() 在 WindowEdge 时自动 `record_final`(PCM→archive,三份文本→day log+ring)。
    storage: Option<Arc<Storage>>,
}

impl Pipeline {
    /// Compose an already-built Stage1 + Stage2 (no storage recording). 低层入口 ——
    /// [`Self::assemble`] 是带选型拼装的高层入口;示例(bench)用这个。
    pub fn new(s1: OnnxStage1Recognizer, s2: Box<dyn Stage2Calibrator>) -> Self {
        Self { s1, s2, storage: None }
    }

    /// 全栈拼装:spec → Stage1(ONNX recognizer,含模型加载+scout ingest 线程)+
    /// Stage2(local Calibrator 预热 / remote HttpLlm),接共享热词/纠偏 store。
    /// `active` = scout 连接开关(socket 共享翻转);`storage` = Some 时 run() 内自动归档。
    /// 模型选择日志(VAD/ASR backend/LLM)在此打出。
    pub fn assemble(
        spec: &PipelineSpec,
        active: Arc<AtomicBool>,
        hotwords: Arc<Mutex<Vec<String>>>,
        corrections: Arc<Mutex<Vec<(String, String)>>>,
        storage: Option<Arc<Storage>>,
    ) -> Result<Self> {
        info!("loading Stage1 (ONNX) + Stage2 (Qwen calibrator) …");
        let s1 = OnnxStage1Recognizer::new(stage1_config(spec, active)?)?;
        let s2 = stage2_calibrator(spec, hotwords, corrections)?;
        Ok(Self { s1, s2, storage })
    }

    /// Run the pipeline. Blocks forever (Stage1 consume loop never returns).
    pub fn run<F>(self, on_turn: F) -> !
    where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        let Pipeline { s1, s2, storage } = self;
        let on_turn = Arc::new(on_turn);

        // Stage2 worker on its own thread — drains the two Stage1 triggers off-channel.
        // Events arrive in order on this single thread (Batch×N → WindowEdge), so Stage2's
        // tiny window state (last calibration per window) can never desync.
        let (tx, rx) = mpsc::channel::<Stage1Event>();
        {
            let on_turn = Arc::clone(&on_turn);
            let mut s2 = s2;
            thread::Builder::new()
                .name("aura-stage2".into())
                .spawn(move || {
                    for ev in rx {
                        match ev {
                            Stage1Event::Batch { window_id, segments } => {
                                let t = Instant::now();
                                let calibrated = s2.calibrate_window(window_id, &segments);
                                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                                // 联合整流当前窗口(每 VAD gap 一次)——高频,debug。
                                debug!(
                                    window_id,
                                    route_ms = route_ms.round() as u64,
                                    calibrated = %calibrated,
                                    "纠偏[segment]"
                                );
                                on_turn(TurnEvent::WindowCalibrated {
                                    window_id,
                                    calibrated,
                                    route_ms,
                                });
                            }
                            Stage1Event::WindowEdge { window } => {
                                let t = Instant::now();
                                // 定稿不跑 LLM:取该窗口最后一次 Batch 联合整流的存档
                                // (最后一个段到来时整流已完成),移动左边界。
                                let calibrated = s2.finalize_window(&window);
                                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                                // Log all three text layers — window-level batch (authoritative;
                                // empty = re-run failed), the streaming concat, and the Stage2
                                // rewrite — so ASR-level loss is distinguishable from LLM rewriting.
                                info!(
                                    window_id = window.id,
                                    at_s = (window.start_s * 10.0).round() / 10.0,
                                    segs = window.segments.len(),
                                    route_ms = route_ms.round() as u64,
                                    batch = %window.batch_text.clone().unwrap_or_default(),
                                    streaming = %window.streaming_text,
                                    calibrated = %calibrated,
                                    "纠偏[window]"
                                );
                                // 归档:窗口 PCM → audio archive,三份文本 → day log + ring
                                // (backs /api/audio + /api/recordings)。
                                if let Some(storage) = &storage {
                                    storage.record_final(FinalTurn {
                                        window_id: window.id,
                                        at_s: window.start_s,
                                        duration_ms: window.duration_ms(),
                                        raw_text: window.batch_text.clone().unwrap_or_default(),
                                        streaming_text: window.streaming_text.clone(),
                                        calibrated: calibrated.clone(),
                                        route_ms,
                                        pcm: (*window.pcm).clone(),
                                    });
                                }
                                on_turn(TurnEvent::WindowFinal {
                                    window: &window,
                                    calibrated,
                                    route_ms,
                                });
                            }
                            Stage1Event::Interim { .. } => {
                                // Never sent down the channel (the Stage1 loop handles Interim
                                // inline) — defensive no-op if that ever changes.
                            }
                        }
                    }
                })
                .expect("spawn aura-stage2 worker");
        }

        // Stage1 consume loop (this thread) — Interim partials pass straight through; the two
        // Stage2 triggers are handed to the worker so this loop never blocks on the LLM.
        s1.run(&mut move |ev| match ev {
            Stage1Event::Interim { window_id, segment_id, partial, at_s } => {
                // 高频(说话中 ~0.5s/条)——debug;aura.yaml `log_level: debug` 打开。
                debug!(
                    window_id,
                    segment_id,
                    at_s = (at_s * 10.0).round() / 10.0,
                    partial = %partial,
                    "流式"
                );
                on_turn(TurnEvent::Interim {
                    window_id,
                    segment_id,
                    partial: &partial,
                    at_s,
                });
            }
            ev @ (Stage1Event::Batch { .. } | Stage1Event::WindowEdge { .. }) => {
                if tx.send(ev).is_err() {
                    tracing::error!("stage2 worker gone — dropping event");
                }
            }
        });
    }

    /// 在专用 `aura-pipeline` std 线程上启动(daemon 布局:主线程留给 tokio socket)。
    /// 语义与 [`Self::run`] 相同,只是不占调用线程。
    pub fn spawn<F>(self, on_turn: F) -> Result<thread::JoinHandle<()>>
    where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        Ok(thread::Builder::new()
            .name("aura-pipeline".into())
            .spawn(move || {
                self.run(on_turn); // returns `!` — this thread never exits
            })?)
    }
}

/// 纯映射(除 Stage1Config::new 的路径解析 R6 TODO 外无重 IO):spec → Stage1Config。
/// ASR 后端选择分支与全部模型选择日志都在这里;流式引擎/未知选型在这里报错。
/// 单测直接盖这个函数。
fn stage1_config(spec: &PipelineSpec, active: Arc<AtomicBool>) -> Result<Stage1Config> {
    // 流式引擎(恒本地):zipformer 是当前唯一实现。
    if spec.stream.model != "zipformer" {
        bail!(
            "unsupported asr.stream.model {:?} (supported: \"zipformer\")",
            spec.stream.model
        );
    }
    // 自定义模型根目录:local 路径下 VAD/流式/批式全部改在其下解析。
    // (remote/disabled 批式时流式/VAD 仍走 MODELS 命名空间 —— model_dir 是 local 旋钮。)
    let model_dir = match &spec.asr {
        AsrSpec::Local { model_dir, .. } => model_dir.clone(),
        AsrSpec::Remote { .. } | AsrSpec::Disabled => None,
    };
    let mut cfg = Stage1Config::with_models_dir(spec.scout_addr.clone(), model_dir);
    cfg.active = active;
    // VAD / 分段(默认 = VadSpec::default,与 Stage1Config 内置默认一致)。
    let v = &spec.vad;
    cfg.vad.threshold = v.threshold;
    cfg.vad.min_silence_duration = v.min_silence;
    cfg.vad.min_speech_duration = v.min_speech;
    cfg.vad.max_speech_duration = v.max_speech;
    cfg.vad.edge_margin_s = v.edge_margin;
    cfg.merge_gap_s = v.merge_gap;
    info!(
        threshold = v.threshold,
        min_silence_s = v.min_silence,
        merge_gap_s = v.merge_gap,
        edge_margin_s = ((v.edge_margin as f64) * 1000.0).round() / 1000.0, // f32 can't hold 0.3 — round in f64 for a clean display
        "VAD: min_silence 切段 + merge_gap 合并碎片 + edge_margin 补边界 (解耦)"
    );
    // Bake the seed hotwords into the streaming recognizer (beam-search biasing).
    cfg.streaming.hotwords = spec.hotwords.clone();
    // Select batch ASR backend (default: SenseVoice).
    //   "whisper"   → large-v3-turbo
    //   "qwen3-asr" → Qwen3-Audio ASR 1.7B int8 (high accuracy, slow on CPU)
    Ok(match &spec.asr {
        AsrSpec::Remote { endpoint } => {
            info!("ASR: remote HTTP {endpoint}");
            cfg.with_remote_asr(endpoint.clone())
        }
        AsrSpec::Disabled => {
            info!("ASR batch: disabled — streaming-only (batch_text 恒 None,回退流式文本)");
            cfg.batch_enabled = false;
            cfg
        }
        AsrSpec::Local { backend, language, hardware: provider, threads, .. } => {
            if backend == "whisper" {
                info!("ASR backend: Whisper large-v3-turbo (language: {language})");
                cfg = cfg.with_whisper_asr(language);
            } else if backend == "qwen3-asr" {
                info!("ASR backend: Qwen3-Audio ASR 1.7B int8");
                cfg = cfg.with_qwen3_asr();
            } else {
                info!("ASR backend: SenseVoice (language: {language})");
            }
            // Batch-ASR ONNX provider (VAD + streaming stay CPU). cuDNN 9.25+ for sm_120 numerics.
            cfg.asr.provider = provider.clone();
            cfg.asr.num_threads = *threads;
            if backend == "qwen3-asr" && provider == "cuda" {
                info!("Qwen3-ASR on CUDA: correct (cuDNN 9.25) but autoregressive ⇒ ~CPU speed");
            }
            info!(
                "ASR provider: {} | threads: {} (batch ASR; VAD + streaming on CPU)",
                cfg.asr.provider,
                cfg.asr.num_threads
            );
            cfg
        }
    })
}

/// Stage2 组装:local mistral.rs Calibrator(加载 + "你好"预热)或 remote HttpLlm,
/// 包成 Stage2CalibratorImpl 并接共享热词/纠偏 store。
fn stage2_calibrator(
    spec: &PipelineSpec,
    hotwords: Arc<Mutex<Vec<String>>>,
    corrections: Arc<Mutex<Vec<(String, String)>>>,
) -> Result<Box<dyn Stage2Calibrator>> {
    let llm: Arc<dyn dp_models::LlmProvider> = match &spec.llm {
        LlmSpec::Remote { endpoint, model } => {
            info!("Stage2 LLM: remote HTTP {endpoint} (model {model})");
            Arc::new(HttpLlm::new(endpoint.clone(), model.clone()))
        }
        LlmSpec::Local { model, model_dir } => {
            let calibrator = match model_dir {
                Some(dir) => Calibrator::load(dir, model)?,
                None => Calibrator::load_default(model)?,
            };
            let _ = calibrator.calibrate_blocking("你好", None, &[]); // HF warmup
            info!("Stage2 LLM: local mistral.rs ({model})");
            Arc::new(calibrator)
        }
    };
    Ok(Box::new(Stage2CalibratorImpl::new(llm, hotwords, corrections)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dp_models::onnx::AsrBackend;
    use dp_models::ProviderKind;
    use std::sync::atomic::Ordering;

    fn spec(asr: AsrSpec) -> PipelineSpec {
        PipelineSpec {
            scout_addr: "127.0.0.1:7878".into(),
            hotwords: vec!["Rust".into()],
            vad: VadSpec::default(),
            stream: StreamSpec { model: "zipformer".into() },
            asr,
            llm: LlmSpec::Local { model: "m.gguf".into(), model_dir: None },
        }
    }

    fn local(backend: &str) -> AsrSpec {
        AsrSpec::Local {
            backend: backend.into(),
            language: "auto".into(),
            hardware: "cpu".into(),
            threads: 8,
            model_dir: None,
        }
    }

    #[test]
    fn stage1_config_selects_backend_per_spec() {
        // remote → batch ASR 走 HTTP(流式/VAD 仍本地)。
        let cfg = stage1_config(
            &spec(AsrSpec::Remote { endpoint: "http://127.0.0.1:8000".into() }),
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        assert!(matches!(cfg.asr_kind, ProviderKind::Remote { .. }));
        assert!(cfg.batch_enabled, "remote batch stays on");

        // whisper / qwen3-asr → 对应 ONNX 后端。
        let cfg = stage1_config(&spec(local("whisper")), Arc::new(AtomicBool::new(true))).unwrap();
        assert!(matches!(cfg.asr.backend, AsrBackend::Whisper { .. }));
        let cfg = stage1_config(&spec(local("qwen3-asr")), Arc::new(AtomicBool::new(true))).unwrap();
        assert!(matches!(cfg.asr.backend, AsrBackend::Qwen3Asr { .. }));

        // 未知/默认 → SenseVoice;provider/threads 落位。
        let cfg = stage1_config(&spec(local("sensevoice")), Arc::new(AtomicBool::new(true))).unwrap();
        assert!(matches!(cfg.asr.backend, AsrBackend::SenseVoice { .. }));
        assert_eq!(cfg.asr.provider, "cpu");
        assert_eq!(cfg.asr.num_threads, 8);
    }

    #[test]
    fn stage1_config_disabled_batch_and_unknown_stream_rejected() {
        // disable → 纯流式:batch_enabled=false(不加载批式模型,DisabledAsr 顶位)。
        let cfg = stage1_config(&spec(AsrSpec::Disabled), Arc::new(AtomicBool::new(true))).unwrap();
        assert!(!cfg.batch_enabled);
        assert!(matches!(cfg.asr_kind, ProviderKind::Local { .. }), "不影响 streaming/VAD 的本地路径");

        // 未知流式引擎 → 显式报错(不静默回退默认)。
        let mut s = spec(local("sensevoice"));
        s.stream.model = "bogus".into();
        assert!(stage1_config(&s, Arc::new(AtomicBool::new(true))).is_err());
    }

    #[test]
    fn stage1_config_applies_vad_hotwords_and_active() {
        let mut s = spec(local("sensevoice"));
        s.vad.threshold = 0.6;
        s.vad.merge_gap = 2.5;
        let active = Arc::new(AtomicBool::new(false));
        let cfg = stage1_config(&s, Arc::clone(&active)).unwrap();
        assert!((cfg.vad.threshold - 0.6).abs() < 1e-6);
        assert_eq!(cfg.merge_gap_s, 2.5);
        assert_eq!(cfg.streaming.hotwords, vec!["Rust".to_string()], "seed baked into streaming");
        assert!(!cfg.active.load(Ordering::Relaxed), "shared toggle wired through");
    }

    #[test]
    fn stage1_config_rebases_paths_under_model_dir() {
        // model_dir 设置后,VAD/默认批式/后端 builder 的路径全部改在其下解析。
        let mut s = spec(local("sensevoice"));
        if let AsrSpec::Local { model_dir, .. } = &mut s.asr {
            *model_dir = Some("/custom/models".into());
        }
        let cfg = stage1_config(&s, Arc::new(AtomicBool::new(true))).unwrap();
        assert!(cfg.vad.model.starts_with("/custom/models/silero-vad/"));
        assert!(matches!(&cfg.asr.backend, AsrBackend::SenseVoice { model, .. }
            if model.starts_with("/custom/models/sensevoice/")));

        let mut s = spec(local("whisper"));
        if let AsrSpec::Local { model_dir, .. } = &mut s.asr {
            *model_dir = Some("/m".into());
        }
        let cfg = stage1_config(&s, Arc::new(AtomicBool::new(true))).unwrap();
        assert!(matches!(&cfg.asr.backend, AsrBackend::Whisper { encoder, .. }
            if encoder.starts_with("/m/whisper/")), "builder 走同一根目录");
    }

    #[test]
    fn vad_spec_defaults_match_stage1_config() {
        // 防漂移:VadSpec::default 必须逐字段等于 Stage1Config::new 的内置默认
        // (assemble 直接覆盖 cfg.vad,daemon 用 default 作 resolve 兜底——两处都依赖它)。
        let d = VadSpec::default();
        let cfg = Stage1Config::new("x");
        assert!((d.threshold - cfg.vad.threshold).abs() < 1e-6);
        assert!((d.min_silence - cfg.vad.min_silence_duration).abs() < 1e-6);
        assert!((d.min_speech - cfg.vad.min_speech_duration).abs() < 1e-6);
        assert!((d.max_speech - cfg.vad.max_speech_duration).abs() < 1e-6);
        assert!((d.edge_margin - cfg.vad.edge_margin_s).abs() < 1e-6);
        assert_eq!(d.merge_gap, cfg.merge_gap_s);
    }
}
