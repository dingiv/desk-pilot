//! pipeline (原 composer) — the `Pipeline` (组装车间): wires [`Stage1Recognizer`] →
//! [`Stage2Calibrator`] and emits [`TurnEvent`]s to a caller-supplied callback. Pure
//! orchestration — it does no printing, no file I/O, no Stage3 logic.
//!
//! **拼装也在这里**: [`PipelineSpec`] 是"选什么模型/什么参数"的完整描述（daemon 的
//! resolve() 产出），[`Pipeline::assemble`] 把它变成可运行的 Pipeline —— Stage1Config
//! 逐字段落位 + ASR 后端选择 + Stage2 LLM 选择（local mistral.rs / remote HTTP）+ 模型
//! 加载与预热。daemon 只负责 config 解析、socket 和 Stage3 触发；识别事件日志
//! （流式/纠偏）与段落归档（[`Storage::record_final`]）也在 run() 内部，不劳调用方。
//!
//! Stage2 calibration runs on its own `aura-stage2` worker thread so the Stage1 consume loop
//! never blocks on the LLM — streaming partials keep flowing while a paragraph is being
//! calibrated. A `StreamFragment` for sentence N+1 can arrive BEFORE the `ParagraphCalibration`
//! for paragraph N.
//!
//! The worker drains the two Stage1 triggers off an mpsc channel (`StreamFragment` never
//! crosses the channel — it passes straight through on the Stage1 thread): `Batch` →
//! [`Stage2Calibrator::calibrate_paragraph`] (joint calibration of the current paragraph, result
//! overwrites the paragraph's stored calibration) → [`TurnEvent::SentenceCalibration`];
//! `ParagraphEdge` → [`Stage2Calibrator::finalize_paragraph`] (NO LLM — attach the stored
//! calibration as the paragraph's final field, move the left boundary) →
//! [`TurnEvent::ParagraphCalibration`]. The worker also surfaces the two batch layers as
//! [`TurnEvent::BatchSentence`] (the just-closed sentence's batch) and [`TurnEvent::BatchParagraph`]
//! (the whole-paragraph re-run).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info};

use crate::calibrator::{LlmInput, PassThroughCalibrator, Stage2Calibrator, Stage2CalibratorImpl};
use crate::hub::{FinalTurn, Storage};
use crate::recognizer::{OnnxStage1Recognizer, Stage1Config, Stage1Recognizer};
use crate::Stage1Event;
use dp_models::http::HttpLlm;

// ── PipelineSpec — 选型描述（daemon resolve() 产出，assemble() 消费）────────────────
// 分层:daemon 负责"从哪儿读配置"(yaml/json/CLI/默认值),这里只认 fully-resolved 的
// 具体值 —— 线协议/文件格式不进 core。VadSpec::default 与 Stage1Config::new 的内置
// 默认一致(单测钉死,防两处漂移)。

/// Fully-resolved pipeline 选型:音频源、种子热词、VAD/分句参数、流式 ASR、Stage1 batch
/// ASR、Stage2 LLM。[`Pipeline::assemble`] 的唯一输入(运行时共享句柄除外)。
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineSpec {
    /// omni-scout `/audio` 地址。
    pub scout_addr: String,
    /// 客户端请求 scout 的推流 cadence(ms):`?chunk_ms=N` 让 scout 把源 buffer 聚合成
    /// N-ms 的 HTTP chunk 再推(不能快过 scout 的 node.quantum)。None = 不传参,scout
    /// 按自身 quantum 速率推。纯网络层优化——daemon 侧照样重切成 32ms 窗。
    pub scout_chunk_ms: Option<u64>,
    /// 种子热词:烘烤进流式 recognizer(beam bias),并预载 Stage2 共享 store。
    pub hotwords: Vec<String>,
    pub vad: VadSpec,
    pub stream: StreamSpec,
    pub asr: AsrSpec,
    pub llm: LlmSpec,
    /// Stage2 纠偏的输入源（`llm.input`）：batch（默认）| stream | both。
    pub llm_input: LlmInput,
}

/// 流式 ASR 选型(**恒本地** —— 实时 partial 要低延迟,不走 remote)。当前唯一引擎
/// zipformer;新引擎落地时在 [`stage1_config`] 的 match 里扩臂。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSpec {
    /// "zipformer" (当前唯一;未知值 assemble 直接报错)。
    pub model: String,
}

/// VAD/分句参数(具体值)。[`Default`] 与 [`Stage1Config::new`] 的内置默认逐字段一致。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadSpec {
    /// Silero speech-probability threshold(0.5)。高=不敏感,低=易误触。
    pub threshold: f32,
    /// 切句间隔秒(1.0)——短于此的停顿不切句。
    pub min_silence: f32,
    /// 短于该时长的句被 Silero 丢弃(0.3)。
    pub min_speech: f32,
    /// 超长强切兜底秒(28.0)。
    pub max_speech: f32,
    /// ★merge 段落间隔秒(5.0)——"什么算一句话"的上界;0 = 每句独立成窗。
    pub merge_gap: f64,
    /// 句边界扩展秒(0.3;0=off)——补 Silero 切掉的软起音/尾音。
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
    /// 远程 HTTP(OpenAI 兼容 `/v1/audio/transcriptions`)。`endpoint` = base URL,
    /// `model` = 服务端模型名(必须与 dp-router.yaml `models[].name` 对齐;OpenAI 规范
    /// 要求 multipart form 里带 `model` 字段)。流式/VAD 仍走 MODELS 命名空间。
    Remote { endpoint: String, model: String },
    /// 批式整体禁用(纯流式模式):不加载批式模型,`batch_text` 恒 `None` —— 消费方
    /// 按设计回退流式文本。省掉句级/段落级 batch 调用(远程 ~3.5s/次)。
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
    /// 远程 HTTP(OpenAI 兼容 `/v1/chat/completions`,目标为 dp-router 或 vLLM / SGLang / 任意
    /// OpenAI 兼容服务)。`model` = 服务端模型名;`endpoint` = base URL(不带 `/v1`)。
    Remote { endpoint: String, model: String },
    /// Stage2 整体禁用:不加载任何 LLM,校准 = 恒等(`calibrated` 直接承载原文)。
    /// 纯 ASR 部署 / 对照 Stage2 贡献用。
    Disabled,
}

impl LlmSpec {
    /// "remote" | "disabled" — 配置快照(ConfigView)的显示标签。
    pub fn kind(&self) -> &'static str {
        match self {
            LlmSpec::Remote { .. } => "remote",
            LlmSpec::Disabled => "disabled",
        }
    }
}

/// One turn surfaced to the caller. Data-plane event vocabulary (mirrors `AsrEvent`):
/// [`StreamFragment`](TurnEvent::StreamFragment) + [`BatchSentence`](TurnEvent::BatchSentence) +
/// [`BatchParagraph`](TurnEvent::BatchParagraph) from Stage1; [`SentenceCalibration`](TurnEvent::SentenceCalibration)
/// + [`ParagraphCalibration`](TurnEvent::ParagraphCalibration) from Stage2.
#[derive(Debug)]
pub enum TurnEvent<'a> {
    /// Live streaming output for the CURRENT sentence (raw, uncalibrated). Straight from the
    /// Stage1 thread — NOT a Stage2 input (D2: no live-partial calibration).
    StreamFragment { paragraph_id: u64, sentence_id: u64, text: &'a str, at_s: f64 },
    /// The just-closed sentence's batch pass (per-sentence batch, at EOS).
    BatchSentence { paragraph_id: u64, sentence_id: u64, text: String },
    /// The whole-paragraph batch re-run (per ParagraphEdge) — authoritative raw_text.
    BatchParagraph { paragraph_id: u64, text: String },
    /// Stage2's provisional JOINT calibration of the current paragraph (per Batch) — the
    /// calibrated text so far, replacing the previous calibration of the same paragraph.
    SentenceCalibration { paragraph_id: u64, calibrated: String, route_ms: f64 },
    /// The settled paragraph's final calibration (per ParagraphEdge) — the paragraph's LAST joint
    /// calibration attached as its field (no extra LLM run). Paragraph-granularity final (D3).
    ParagraphCalibration { paragraph_id: u64, calibrated: String, route_ms: f64 },
}

pub struct Pipeline {
    s1: OnnxStage1Recognizer,
    s2: Box<dyn Stage2Calibrator>,
    /// Some → run() 在 ParagraphEdge 时自动 `record_final`(PCM→archive,三份文本→day log+ring)。
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
    /// `active` = scout 连接开关(socket 共享翻转);`running` = idle 深度睡眠信号(run 据此退出
    /// 消费循环, daemon 恢复时置回 true);`storage` = Some 时 run() 内自动归档。
    /// 模型选择日志(VAD/ASR backend/LLM)在此打出。
    pub fn assemble(
        spec: &PipelineSpec,
        active: Arc<AtomicBool>,
        running: Arc<AtomicBool>,
        flush_paragraph: Arc<AtomicBool>,
        hotwords: Arc<Mutex<Vec<String>>>,
        corrections: Arc<Mutex<Vec<(String, String)>>>,
        storage: Option<Arc<Storage>>,
    ) -> Result<Self> {
        info!("loading Stage1 (ONNX) + Stage2 (Qwen calibrator) …");
        let s1 = OnnxStage1Recognizer::new(stage1_config(spec, active, running, flush_paragraph)?)?;
        let s2 = stage2_calibrator(spec, hotwords, corrections)?;
        Ok(Self { s1, s2, storage })
    }

    /// Run the pipeline. Stage2 worker 常驻;Stage1 消费循环在 `running` 置 false(idle 深度睡眠)
    /// 时退出, 在 `resume` condvar 被唤醒(daemon 置 running=true)后重跑。
    pub fn run<F>(
        self,
        running: Arc<AtomicBool>,
        resume: Arc<(Mutex<()>, Condvar)>,
        on_turn: F,
    ) -> !
    where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        let Pipeline { s1, s2, storage } = self;
        let on_turn = Arc::new(on_turn);

        // Stage2 worker on its own thread — drains the two Stage1 triggers off-channel.
        // Events arrive in order on this single thread (Batch×N → ParagraphEdge), so Stage2's
        // tiny paragraph state (last calibration per paragraph) can never desync.
        let (tx, rx) = mpsc::channel::<Stage1Event>();
        {
            let on_turn = Arc::clone(&on_turn);
            let mut s2 = s2;
            thread::Builder::new()
                .name("aura-stage2".into())
                .spawn(move || {
                    for ev in rx {
                        match ev {
                            Stage1Event::Batch { paragraph_id, sentences } => {
                                // BatchSentence: the just-closed sentence's batch text (per-sentence
                                // batch ran synchronously at EOS). `sentences.last()` IS that
                                // sentence. Skipped when batch produced nothing (None).
                                if let Some(sentence) = sentences.last() {
                                    if let Some(text) = sentence.batch_text.clone() {
                                        on_turn(TurnEvent::BatchSentence {
                                            paragraph_id,
                                            sentence_id: sentence.id,
                                            text,
                                        });
                                    }
                                }
                                let t = Instant::now();
                                let calibrated = s2.calibrate_paragraph(paragraph_id, &sentences);
                                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                                // 联合整流当前段落(每 VAD gap 一次)——高频,debug。
                                info!(
                                    paragraph_id,
                                    route_ms = route_ms.round() as u64,
                                    calibrated = %calibrated,
                                    "纠偏[sentence]"
                                );
                                on_turn(TurnEvent::SentenceCalibration {
                                    paragraph_id,
                                    calibrated,
                                    route_ms,
                                });
                            }
                            Stage1Event::ParagraphEdge { paragraph } => {
                                // BatchParagraph: the whole-paragraph batch re-run (authoritative
                                // raw_text). Skipped when the re-run produced nothing (None —
                                // single-sentence paragraphs reuse the sentence's own None too).
                                if let Some(text) = paragraph.batch_text.clone() {
                                    on_turn(TurnEvent::BatchParagraph {
                                        paragraph_id: paragraph.id,
                                        text,
                                    });
                                }
                                let t = Instant::now();
                                // 定稿不跑 LLM:取该段落最后一次 Batch 联合整流的存档
                                // (最后一个句到来时整流已完成),移动左边界。
                                let calibrated = s2.finalize_paragraph(&paragraph);
                                let route_ms = t.elapsed().as_secs_f64() * 1000.0;
                                // Log all three text layers — paragraph-level batch (authoritative;
                                // empty = re-run failed), the streaming concat, and the Stage2
                                // rewrite — so ASR-level loss is distinguishable from LLM rewriting.
                                info!(
                                    paragraph_id = paragraph.id,
                                    at_s = (paragraph.start_s * 10.0).round() / 10.0,
                                    sentences = paragraph.sentences.len(),
                                    // batch 模型调用耗时;单句复用模式 = 0(不打点,见 asr_ms 段日志)。
                                    asr_ms = paragraph.batch_asr_ms,
                                    route_ms = route_ms.round() as u64,
                                    batch = %paragraph.batch_text.clone().unwrap_or_default(),
                                    streaming = %paragraph.streaming_text,
                                    calibrated = %calibrated,
                                    "纠偏[paragraph]"
                                );
                                // 归档:段落 PCM → audio archive,三份文本 → day log + ring
                                // (backs /api/audio + /api/recordings)。
                                if let Some(storage) = &storage {
                                    storage.record_final(FinalTurn {
                                        paragraph_id: paragraph.id,
                                        at_s: paragraph.start_s,
                                        duration_ms: paragraph.duration_ms(),
                                        raw_text: paragraph.batch_text.clone().unwrap_or_default(),
                                        streaming_text: paragraph.streaming_text.clone(),
                                        calibrated: calibrated.clone(),
                                        route_ms,
                                        pcm: (*paragraph.pcm).clone(),
                                    });
                                }
                                on_turn(TurnEvent::ParagraphCalibration {
                                    paragraph_id: paragraph.id,
                                    calibrated,
                                    route_ms,
                                });
                            }
                            Stage1Event::StreamFragment { .. } => {
                                // Never sent down the channel (the Stage1 loop handles
                                // StreamFragment inline) — defensive no-op if that ever changes.
                            }
                        }
                    }
                })
                .expect("spawn aura-stage2 worker");
        }

        // Stage1 consume loop (this thread) — StreamFragment partials pass straight through; the two
        // Stage2 triggers are handed to the worker so this loop never blocks on the LLM.
        // idle 深度睡眠: running=false 时 run() 返回; 等 daemon 恢复(running=true + notify)后重跑。
        loop {
            let tx = tx.clone();
            let on_turn = Arc::clone(&on_turn);
            s1.run(&mut move |ev| match ev {
            Stage1Event::StreamFragment { paragraph_id, sentence_id, text, at_s } => {
                // 高频(说话中 ~0.5s/条)——debug;aura.yaml `log_level: debug` 打开。
                debug!(
                    paragraph_id,
                    sentence_id,
                    at_s = (at_s * 10.0).round() / 10.0,
                    text = %text,
                    "流式"
                );
                on_turn(TurnEvent::StreamFragment {
                    paragraph_id,
                    sentence_id,
                    text: &text,
                    at_s,
                });
            }
            ev @ (Stage1Event::Batch { .. } | Stage1Event::ParagraphEdge { .. }) => {
                if tx.send(ev).is_err() {
                    tracing::error!("stage2 worker gone — dropping event");
                }
            }
        });
            // 深度睡眠: idle 后等待 daemon 恢复(running=true + notify), 再重跑消费循环。
            let (lock, cv) = &*resume;
            let mut guard = lock.lock().unwrap();
            while !running.load(Ordering::Relaxed) {
                guard = cv.wait(guard).unwrap();
            }
        }
    }

    /// 在专用 `aura-pipeline` std 线程上启动(daemon 布局:主线程留给 tokio socket)。
    /// 语义与 [`Self::run`] 相同,只是不占调用线程。
    pub fn spawn<F>(
        self,
        running: Arc<AtomicBool>,
        resume: Arc<(Mutex<()>, Condvar)>,
        on_turn: F,
    ) -> Result<thread::JoinHandle<()>>
    where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        Ok(thread::Builder::new()
            .name("aura-pipeline".into())
            .spawn(move || {
                self.run(running, resume, on_turn); // returns `!` — this thread never exits
            })?)
    }
}

/// 纯映射(除 Stage1Config::new 的路径解析 R6 TODO 外无重 IO):spec → Stage1Config。
/// ASR 后端选择分支与全部模型选择日志都在这里;流式引擎/未知选型在这里报错。
/// 单测直接盖这个函数。
fn stage1_config(
    spec: &PipelineSpec,
    active: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    flush_paragraph: Arc<AtomicBool>,
) -> Result<Stage1Config> {
    // 自定义模型根目录:local 路径下 VAD/流式/批式全部改在其下解析。
    // (remote/disabled 批式时流式/VAD 仍走 MODELS 命名空间 —— model_dir 是 local 旋钮。)
    let model_dir = match &spec.asr {
        AsrSpec::Local { model_dir, .. } => model_dir.clone(),
        AsrSpec::Remote { .. } | AsrSpec::Disabled => None,
    };
    let mut cfg = Stage1Config::with_models_dir(spec.scout_addr.clone(), model_dir);
    cfg.active = active;
    cfg.running = running;
    cfg.flush_paragraph = flush_paragraph;
    // 客户端请求的 scout 推流 cadence(ms):None = scout 按自身 quantum 速率推。
    cfg.scout_chunk_ms = spec.scout_chunk_ms;
    // VAD / 分句(默认 = VadSpec::default,与 Stage1Config 内置默认一致)。
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
        "VAD: min_silence 切句 + merge_gap 合并碎片 + edge_margin 补边界 (解耦)"
    );
    // 流式引擎(恒本地):zipformer(默认) | x-asr。路径在 recognizer 侧解析,
    // 未知引擎在那里报错(不静默回退)。
    cfg = cfg.with_stream_engine(&spec.stream.model)?;
    // Bake the seed hotwords into the streaming recognizer (beam-search biasing). MUST run
    // after the engine selection — with_stream_engine replaces the whole streaming config.
    cfg.streaming.hotwords = spec.hotwords.clone();
    // Select batch ASR backend (default: SenseVoice).
    //   "whisper"   → large-v3-turbo
    //   "qwen3-asr" → Qwen3-Audio ASR 1.7B int8 (high accuracy, slow on CPU)
    Ok(match &spec.asr {
        AsrSpec::Remote { endpoint, model } => {
            info!("ASR: remote HTTP {endpoint} (model {model})");
            cfg.with_remote_asr(endpoint.clone(), model.clone())
        }
        AsrSpec::Disabled => {
            info!("ASR batch: disabled — streaming-only (batch_text 恒 None,回退流式文本)");
            cfg.batch_enabled = false;
            cfg
        }
        AsrSpec::Local { backend, language, hardware: provider, threads, .. } => {
            // 本地 batch 只保留 SenseVoice —— whisper / qwen3-asr 的本地模型已删
            // (qwen3-asr 改走 remote, 见 README)。配置它们直接报错, 不静默回退。
            if backend != "sensevoice" {
                anyhow::bail!(
                    "asr.local.model: {backend} 不支持——本地批式仅 sensevoice \
                     (whisper/qwen3-asr 模型已删; qwen3-asr 请用 asr.backend: remote)"
                );
            }
            info!("ASR backend: SenseVoice (language: {language})");
            // Batch-ASR ONNX provider (VAD + streaming stay CPU). cuDNN 9.25+ for sm_120 numerics.
            cfg.asr.provider = provider.clone();
            cfg.asr.num_threads = *threads;
            info!(
                "ASR provider: {} | threads: {} (batch ASR; VAD + streaming on CPU)",
                cfg.asr.provider,
                cfg.asr.num_threads
            );
            cfg
        }
    })
}

/// Stage2 组装:remote HttpLlm(指向 dp-router 或任意 OpenAI 兼容上游);
/// 包成 Stage2CalibratorImpl 并接共享热词/纠偏 store。
fn stage2_calibrator(
    spec: &PipelineSpec,
    hotwords: Arc<Mutex<Vec<String>>>,
    corrections: Arc<Mutex<Vec<(String, String)>>>,
) -> Result<Box<dyn Stage2Calibrator>> {
    let llm: Arc<dyn dp_models::LlmProvider> = match &spec.llm {
        LlmSpec::Disabled => {
            info!("Stage2 LLM: disabled — pass-through (calibrated = 原文, 零 LLM)");
            return Ok(Box::new(PassThroughCalibrator));
        }
        LlmSpec::Remote { endpoint, model } => {
            info!("Stage2 LLM: remote HTTP {endpoint} (model {model})");
            Arc::new(HttpLlm::new(endpoint.clone(), model.clone()))
        }
    };
    Ok(Box::new(Stage2CalibratorImpl::new(llm, hotwords, corrections, spec.llm_input)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{VadSentence, VadParagraph};
    use dp_models::onnx::AsrBackend;
    use dp_models::ProviderKind;
    use std::sync::atomic::Ordering;

    fn spec(asr: AsrSpec) -> PipelineSpec {
        PipelineSpec {
            scout_addr: "127.0.0.1:7878".into(),
            scout_chunk_ms: None,
            hotwords: vec!["Rust".into()],
            vad: VadSpec::default(),
            stream: StreamSpec { model: "zipformer".into() },
            asr,
            llm: LlmSpec::Remote { endpoint: "http://127.0.0.1:8080".into(), model: "test-model".into() },
            llm_input: LlmInput::Batch,
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
            &spec(AsrSpec::Remote { endpoint: "http://127.0.0.1:8000".into(), model: "x".into() }),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();
        assert!(matches!(cfg.asr_kind, ProviderKind::Remote { .. }));
        assert!(cfg.batch_enabled, "remote batch stays on");

        // 本地只支持 sensevoice —— whisper / qwen3-asr 本地模型已删, 配置它们显式报错。
        assert!(stage1_config(&spec(local("whisper")), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).is_err());
        assert!(stage1_config(&spec(local("qwen3-asr")), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).is_err());

        // sensevoice → SenseVoice;provider/threads 落位。
        let cfg = stage1_config(&spec(local("sensevoice")), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).unwrap();
        assert!(matches!(cfg.asr.backend, AsrBackend::SenseVoice { .. }));
        assert_eq!(cfg.asr.provider, "cpu");
        assert_eq!(cfg.asr.num_threads, 8);
    }

    #[test]
    fn stage2_disabled_is_pass_through_without_any_llm() {
        // llm.backend: disable → PassThrough:校准 = 原文拼接,定稿 = 段落 best_text,
        // 不加载任何模型(route_ms ≈ 0,calibrated 字段承载原文,下游形状不变)。
        let mut s = spec(local("sensevoice"));
        s.llm = LlmSpec::Disabled;
        let mut s2 = stage2_calibrator(&s, Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new()))).unwrap();
        let sentences = vec![
            VadSentence {
                id: 1,
                audio_id: 1,
                start_s: 0.0,
                end_s: 1.0,
                streaming_text: "流式一".into(),
                batch_text: Some("批式一".into()),
            },
            VadSentence {
                id: 2,
                audio_id: 2,
                start_s: 1.5,
                end_s: 2.5,
                streaming_text: "流式二".into(),
                batch_text: None, // batch 失败 → 回退 streaming
            },
        ];
        assert_eq!(s2.calibrate_paragraph(1, &sentences), "批式一流式二");
        let win = VadParagraph {
            id: 1,
            sentences: sentences,
            start_s: 0.0,
            end_s: 2.5,
            streaming_text: "流式一流式二".into(),
            batch_text: Some("段落批式".into()),
            pcm: std::sync::Arc::new(vec![0i16; 1600]),
            batch_asr_ms: 0,
        };
        assert_eq!(s2.finalize_paragraph(&win), "段落批式", "paragraph batch 优先");
    }

    #[test]
    fn stage1_config_selects_stream_engine() {
        // x-asr → 指向 x-asr 模型目录;热词在引擎选择之后烘烤(整体替换不丢)。
        let mut s = spec(local("sensevoice"));
        s.stream.model = "x-asr".into();
        let cfg = stage1_config(&s, Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).unwrap();
        assert!(cfg.streaming.encoder.ends_with("x-asr/encoder-480ms.onnx"));
        assert!(cfg.streaming.bpe_vocab.ends_with("x-asr/bpe.vocab"));
        assert_eq!(
            cfg.streaming.hotwords,
            vec!["Rust".to_string()],
            "hotwords baked after the engine swap"
        );
    }

    #[test]
    fn stage1_config_disabled_batch_and_unknown_stream_rejected() {
        // disable → 纯流式:batch_enabled=false(不加载批式模型,DisabledAsr 顶位)。
        let cfg = stage1_config(&spec(AsrSpec::Disabled), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).unwrap();
        assert!(!cfg.batch_enabled);
        assert!(matches!(cfg.asr_kind, ProviderKind::Local { .. }), "不影响 streaming/VAD 的本地路径");

        // 未知流式引擎 → 显式报错(不静默回退默认)。
        let mut s = spec(local("sensevoice"));
        s.stream.model = "bogus".into();
        assert!(stage1_config(&s, Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).is_err());
    }

    #[test]
    fn stage1_config_applies_vad_hotwords_and_active() {
        let mut s = spec(local("sensevoice"));
        s.vad.threshold = 0.6;
        s.vad.merge_gap = 2.5;
        let active = Arc::new(AtomicBool::new(false));
        let cfg = stage1_config(&s, Arc::clone(&active), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).unwrap();
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
        let cfg = stage1_config(&s, Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).unwrap();
        assert!(cfg.vad.model.starts_with("/custom/models/silero-vad/"));
        assert!(matches!(&cfg.asr.backend, AsrBackend::SenseVoice { model, .. }
            if model.starts_with("/custom/models/sensevoice/")));

        // whisper 本地模型已删 —— 即使给了 model_dir 也拒绝(而非拼路径)。
        let mut s = spec(local("whisper"));
        if let AsrSpec::Local { model_dir, .. } = &mut s.asr {
            *model_dir = Some("/m".into());
        }
        assert!(stage1_config(&s, Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false))).is_err());
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
