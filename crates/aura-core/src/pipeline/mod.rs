//! pipeline (原 composer) — the `Pipeline` (组装车间): wires [`recognizer::OnnxStage1Recognizer`] →
//! [`Stage2Calibrator`] and emits [`TurnEvent`]s to a caller-supplied callback. Pure
//! orchestration — it does no printing, no file I/O, no Stage3 logic.
//!
//! **拼装也在这里**: [`PipelineSpec`] 是"选什么模型/什么参数"的完整描述（daemon 的
//! resolve() 产出），[`Pipeline::assemble`] 把它变成可运行的 Pipeline —— Stage1Config
//! 逐字段落位 + ASR 后端选择 + Stage2 LLM 选择（local mistral.rs / remote HTTP）+ 模型
//! 加载与预热。daemon 只负责 config 解析、socket 和 Stage3 触发；识别事件日志
//! （流式/纠偏）与段落归档（[`Storage::record_final`]）也在 run() 内部，不劳调用方。
//!
//! **round12 异步化编排**;**round14 线程模型收拢(round14b)** —— run() 内部不声明任何
//! std 线程:唯一剩余的阻塞桥(scout TCP ingest,sync IO)走 runtime blocking pool;
//! **消费循环本体已 async 化**(帧等待 = `tokio::sync::Notify`,VAD/流式推理内联),
//! 主循环就是 run() 这个 future 本身。宿主选择:
//! - **已有 runtime(daemon)**:`rt.spawn(pipeline.run(..))` —— **零专用线程**;
//! - **无 runtime(examples/bench)**:[`Pipeline::spawn`] = 一条专用线程 +
//!   current_thread runtime 驱动 run()。
//!
//! | 载体(round27 文件 = 线程模型) | 运行什么 | 职责 |
//! |---|---|---|
//! | blocking pool ×1 · `front.rs` | `s1.run_ingest()` | scout TCP → VAD 检测 → 门控帧直发流式 + FrontEvent 入队 |
//! | 异步任务 · `loops.rs` `consume_loop` | `consume_loop(&s1, cb)` | 大脑:边界决策(起音即开段,时间戳 id)、Finalize 握手 |
//! | `run` future(daemon: rt 任务 / 独立宿主: 专用线程)· `loops.rs` `main_loop` | `select!` 主循环 | **唯一 on_turn 调用者**:SF/PC 直通 emit;Batch → 句任务;ParagraphEdge → 段任务 |
//! | 句任务 · `batch.rs` | `spawn_blocking(recognize_once)` | 每句 EOS 一个;完成即回传 `BatchSentence` |
//! | live 整流任务 · `batch.rs` | 链式 `spawn_blocking(calibrate_paragraph)` | BS 到达触发(架构要求 batch 完成 → 之后纠偏);回传 `SentenceCalibration` |
//! | 段任务 · `batch.rs` | join 句任务 + live 链尾 → 段重跑 → LLM 定稿 → 归档 | 就绪门 = `join!` 语义;SC 先于 PCal |


use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use tracing::info;

use calibrator::{PassThroughCalibrator, Stage2Calibrator, Stage2CalibratorImpl};

// round23:流水线环节文件集中在 pipeline/ 模块文件夹(mod = 编排,其余 = 环节)。
pub mod calibrator;
pub mod front;
pub(crate) mod loops;
pub mod spec;
pub mod types;
pub mod vad;
pub mod batch;
pub mod resources;
pub mod stream;
pub mod tracker;
use crate::hub::Storage;
use resources::{OnnxStage1Recognizer, Stage1Config};
use spec::{AsrSpec, LlmSpec, PipelineSpec};
use types::TurnEvent;
use dp_models::http::HttpLlm;

use tokio;
use tokio::sync::Notify;

pub struct Pipeline {
    s1: OnnxStage1Recognizer,
    /// Stage2 校准器(round12:Mutex 串行化 —— 段任务并发,LLM 调用保持单飞,
    /// 与旧单线程 Finalizer 语义一致)。
    s2: Arc<Mutex<Box<dyn Stage2Calibrator>>>,
    /// Some → run() 定稿时自动 `record_final`(PCM→archive,三份文本→day log+ring)。
    storage: Option<Arc<Storage>>,
}

impl Pipeline {
    /// Compose an already-built Stage1 + Stage2 (no storage recording). 低层入口 ——
    /// [`Self::assemble`] 是带选型拼装的高层入口;示例(bench)用这个。`batch_rx` 必须来自
    /// `s1` 的构造(`OnnxStage1Recognizer::new` 返回的接收端)。
    pub fn new(s1: OnnxStage1Recognizer, s2: Box<dyn Stage2Calibrator>) -> Self {
        Self { s1, s2: Arc::new(Mutex::new(s2)), storage: None }
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
        // round12 起:batch pass 由 pipeline 的任务结构自建(recognize_once 直调)。
        let cfg = stage1_config(spec, active, running, flush_paragraph)?;
        let s1 = OnnxStage1Recognizer::new(cfg)?;
        let s2 = stage2_calibrator(spec, hotwords, corrections)?;
        Ok(Self { s1, s2: Arc::new(Mutex::new(s2)), storage })
    }

    /// Run the pipeline — 委托 [`crate::pipeline::loops_::main_loop`](两个循环住
    /// loops.rs:大脑消费循环 + select! 主循环;本类型只做组装与生命周期)。
    pub async fn run<F>(
        self,
        running: Arc<AtomicBool>,
        resume: Arc<Notify>,
        on_turn: F,
    ) where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        let Pipeline { s1, s2, storage } = self;
        loops::main_loop(s1, s2, storage, running, resume, on_turn).await;
    }

    /// 无 runtime 宿主(examples / bench)的便捷入口:一条专用 `aura-pipeline`
    /// std 线程 + current_thread runtime 驱动 [`Self::run`](永不完成)。
    /// **daemon 不用它** —— daemon 把 `pipeline.run(..)` 直接 spawn 到自己的
    /// socket runtime 上(零专用线程,round14)。
    pub fn spawn<F>(
        self,
        running: Arc<AtomicBool>,
        resume: Arc<Notify>,
        on_turn: F,
    ) -> Result<thread::JoinHandle<()>>
    where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        Ok(thread::Builder::new()
            .name("aura-pipeline".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("pipeline tokio runtime");
                // run 是无限循环 —— block_on 永不返回,本线程常驻。
                rt.block_on(self.run(running, resume, on_turn));
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

