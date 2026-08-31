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
//! **round12 异步化编排**(与 ime-core IoThread 同构 —— 阻塞线程产事件 → tokio channel →
//! 专用 current_thread runtime `select!` 主循环 → **单点发射**):
//!
//! | 线程/任务 | 运行什么 | 职责 |
//! |---|---|---|
//! | `aura-stage1-ingest` | `s1.run_ingest()` | scout TCP → AudioRing(自动重连) |
//! | `aura-stage1` | `s1.run(cb)` | 消费循环:VAD + 流式 + 边界决策;cb 只把 `Stage1Event` 推进 tokio channel(batch_jobs=false,不投 job) |
//! | `aura-pipeline-async` | `select!` 主循环(current_thread runtime) | **唯一 on_turn 调用者**:StreamFragment/ParagraphClosed 直通 emit;Batch → 句任务;ParagraphEdge → 段任务 |
//! | 句任务 | `spawn_blocking(recognize_once)` | 每句 EOS 一个;完成即回传 `BatchSentence` |
//! | 段任务 | join 句任务 → 段重跑 → LLM 定稿 → 归档 | live 联合整流(每句完成一次,严格在其 batch 后);就绪门 = `join!` 语义 |
//!
//! 时序语义:边界(`ParagraphClosed`)经 s1 通道 FIFO + 主循环按序 emit,必先于下一段
//! 任何事件(结构保证);跨段乱序(段 N 定稿 vs 段 N+1 流式)是物理现实,客户端按
//! `paragraph_id` 修订。旧 Finalizer 就绪门状态机(ready/expected/para_done 计数)
//! 被任务结构取代 —— `run_batch_worker`/`SentenceBatchReady`/`ParagraphBatchReady`
//! 在新编排下不再使用(保留供旧编排/测试)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info};

use crate::calibrator::{LlmInput, PassThroughCalibrator, Stage2Calibrator, Stage2CalibratorImpl};
use crate::hub::{FinalTurn, Storage};
use crate::recognizer::{OnnxStage1Recognizer, Stage1Config, Stage1Recognizer};
use crate::{ParagraphId, SentenceId, Stage1Event, VadParagraph, VadSentence};
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
    /// consume-loop thread — NOT a Stage2 input (D2: no live-partial calibration).
    StreamFragment { paragraph_id: u64, sentence_id: u64, text: &'a str, at_s: f64 },
    /// 段落边界关闭(VAD 大停顿/整段超时,consume-loop 线程直发)。
    ///
    /// **时序不变式(round11 S3)**:本事件与下一段落的第一个 `StreamFragment`
    /// 同在 `aura-pipeline` 线程直发、按 VAD 顺序产出 → wire 上**严格有序**:
    /// `ParagraphClosed(N)` 必先于段落 N+1 的任何事件。客户端收到即知道该段
    /// 文本已定格 —— 边界时序由 server 保证;此后的 `BatchSentence` /
    /// `BatchParagraph` / `ParagraphCalibration`(stage2 线程,可能乱序迟到)
    /// 是按 `paragraph_id` 定位的**修订**,不再是时序依赖。
    ParagraphClosed { paragraph_id: u64 },
    /// A sentence's batch result (per-sentence batch). ASYNC — arrives AFTER that sentence's
    /// `Batch` (when the batch worker finishes; seconds later for remote ASR).
    BatchSentence { paragraph_id: u64, sentence_id: u64, text: String },
    /// The whole-paragraph batch re-run result (authoritative raw_text). ASYNC — arrives at
    /// finalization (after the `ParagraphEdge`); absent for single-sentence paragraphs (they
    /// reuse the sentence-level batch) or a failed re-run.
    BatchParagraph { paragraph_id: u64, text: String },
    /// Stage2's provisional JOINT calibration of the current paragraph (per Batch) — the
    /// calibrated text so far, replacing the previous calibration of the same paragraph.
    SentenceCalibration { paragraph_id: u64, calibrated: String, route_ms: f64 },
    /// The settled paragraph's final calibration — ONE LLM pass over the final best texts
    /// (all sentence batches are in by the readiness gate; the last live calibration may not
    /// have included the final sentence's batch, hence the extra pass). Paragraph-granularity
    /// final (D3).
    ParagraphCalibration { paragraph_id: u64, calibrated: String, route_ms: f64 },
}

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
        // round12 异步化:batch pass 由 pipeline 的 per-paragraph 任务自建
        // (recognize_once 直调),s1 不再投 job 给 batch worker。
        let mut cfg = stage1_config(spec, active, running, flush_paragraph)?;
        cfg.batch_jobs = false;
        let (s1, _batch_rx) = OnnxStage1Recognizer::new(cfg)?;
        let s2 = stage2_calibrator(spec, hotwords, corrections)?;
        Ok(Self { s1, s2: Arc::new(Mutex::new(s2)), storage })
    }

    /// Run the pipeline. 三条工作线程(ingest / batch / stage2)常驻;Stage1 消费循环在
    /// `running` 置 false(idle 深度睡眠)时退出,在 `resume` condvar 被唤醒(daemon 置
    /// running=true)后重跑。
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
        // s1 被消费线程、任务(recognize_once)共享 —— Arc。
        let s1 = Arc::new(s1);

        // tokio current_thread runtime(专用线程)—— 与 ime-core IoThread 同构
        // (round10 范式):阻塞线程产事件 → channel → select! 主循环 → 单点发射。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("pipeline tokio runtime");

        // 事件桥:s1 消费循环(阻塞线程)→ 主循环。unbounded:send 永不阻塞消费循环。
        let (s1_tx, mut s1_rx) = tokio::sync::mpsc::unbounded_channel::<Stage1Event>();
        // 任务产出回传:BatchSentence / SentenceCalibration / BatchParagraph /
        // ParagraphCalibration 全部经它回主循环 —— **on_turn 只被主循环调用**
        // (发射单点,消除旧双线程并发回调的竞态)。
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel::<TurnEvent<'static>>();

        // ── aura-stage1:消费循环线程(流式/VAD/边界;batch_jobs=false 不投 job)──────
        {
            let s1 = Arc::clone(&s1);
            let running = Arc::clone(&running);
            let resume = Arc::clone(&resume);
            thread::Builder::new()
                .name("aura-stage1".into())
                .spawn(move || loop {
                    let s1_tx = s1_tx.clone();
                    s1.run(&mut move |ev| {
                        if s1_tx.send(ev).is_err() {
                            tracing::error!("pipeline main loop gone — dropping stage1 event");
                        }
                    });
                    // 深度睡眠:running=false 时 run() 返回;等 daemon 恢复(置回 true +
                    // notify)后重跑消费循环。
                    let (lock, cv) = &*resume;
                    let mut guard = lock.lock().unwrap();
                    while !running.load(Ordering::Relaxed) {
                        guard = cv.wait(guard).unwrap();
                    }
                })
                .expect("spawn aura-stage1");
        }

        // ── aura-stage1-ingest:scout → ring ──
        {
            let s1 = Arc::clone(&s1);
            thread::Builder::new()
                .name("aura-stage1-ingest".into())
                .spawn(move || s1.run_ingest())
                .expect("spawn aura-stage1-ingest");
        }

        // ── 主循环(tokio 线程):select 两源,单点 emit ──
        {
            thread::Builder::new()
                .name("aura-pipeline-async".into())
                .spawn(move || {
                    rt.block_on(async move {
                        // 每段的句任务 handles(Batch/EOS 时入;ParagraphEdge 时移交段任务 join)。
                        let mut pending: HashMap<
                            ParagraphId,
                            Vec<tokio::task::JoinHandle<SentenceOutcome>>,
                        > = HashMap::new();
                        loop {
                            tokio::select! {
                                ev = s1_rx.recv() => match ev {
                                    Some(Stage1Event::StreamFragment { paragraph_id, sentence_id, text, at_s }) => {
                                        // 高频(说话中 ~0.5s/条)—— debug;直通低延迟路径。
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
                                    Some(Stage1Event::Batch { paragraph_id, sentences, sr }) => {
                                        // 每句 EOS → 句任务(spawn_blocking recognize;
                                        // clip 在 EOS→段关闭之间存活,任务开头即取 Arc)。
                                        let entry = pending.entry(paragraph_id).or_default();
                                        for s in sentences {
                                            let s1c = Arc::clone(&s1);
                                            let turn = turn_tx.clone();
                                            entry.push(tokio::spawn(sentence_task(
                                                s1c, paragraph_id, s.id, s.audio_id, sr, turn,
                                            )));
                                        }
                                    }
                                    Some(Stage1Event::ParagraphEdge { paragraph, sr }) => {
                                        // 边界时序(round11 S3):ParagraphClosed 先于下一段
                                        // 任何事件 —— s1 通道 FIFO + 主循环按序 emit,结构保证。
                                        on_turn(TurnEvent::ParagraphClosed { paragraph_id: paragraph.id });
                                        // 段任务:join 句任务(就绪门)= live 整流 → 段重跑 →
                                        // 定稿 → 归档。
                                        let handles = pending.remove(&paragraph.id).unwrap_or_default();
                                        let s1c = Arc::clone(&s1);
                                        let s2c = Arc::clone(&s2);
                                        let turn = turn_tx.clone();
                                        let run_batch: RunParagraphBatch =
                                            Arc::new(move |pcm, sr, pid| s1c.recognize_once(pcm, sr, "段落级重跑", pid));
                                        tokio::spawn(paragraph_task(
                                            paragraph, sr, handles, s2c, run_batch, storage.clone(), turn,
                                        ));
                                    }
                                    // batch_jobs=false 下不再产生(旧编排变体,防御 no-op)。
                                    Some(Stage1Event::SentenceBatchReady { .. } | Stage1Event::ParagraphBatchReady { .. }) => {}
                                    None => {} // s1 线程深睡后 channel 仍存活,不会 None;防御。
                                },
                                ev = turn_rx.recv() => {
                                    // None = 全部任务结束才会发生;任务随 select 循环存续,防御。
                                    if let Some(t) = ev {
                                        on_turn(t);
                                    }
                                }
                            }
                        }
                    });
                })
                .expect("spawn aura-pipeline-async");
        }

        // 工作已全部交由专用线程;调用线程(spawn 包装)永久驻留满足 `-> !`。
        loop {
            std::thread::park();
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

// ── round12 异步化:per-sentence / per-paragraph 任务(替代 Finalizer 状态机)────────

/// 段级重跑闭包:_blocking recognize(生产 = `s1.recognize_once`;测试注入 stub)。
type RunParagraphBatch = Arc<dyn Fn(&[i16], u32, ParagraphId) -> Option<String> + Send + Sync>;
/// 任务产出回传通道(pipeline 内部;主循环 drain → 单点 emit)。
type TurnTx = tokio::sync::mpsc::UnboundedSender<TurnEvent<'static>>;
//
// 编排模型(与 ime-core IoThread 同构):s1 消费循环(阻塞线程)产 Stage1Event →
// tokio channel;主循环 select! 单点 emit;batch 由任务自建(spawn_blocking 直调
// `recognize_once`)。时序不变式不再靠手写计数门,而是任务结构本身:
//
//   句任务(Batch/EOS 触发)→ 完成即回传 BatchSentence;
//   段任务(ParagraphEdge 触发)→ join 全部句任务(就绪门 = join!)→ live 联合整流
//   (每句完成一次,严格在该句 BatchSentence 之后)→ 段级重跑(多句;单句复用句级)→
//   BatchParagraph → 定稿整流一次 → 归档 + ParagraphCalibration。
//
// 跨段乱序(段 N 定稿 vs 段 N+1 流式)是物理现实 —— 客户端按 paragraph_id 修订。

/// 句任务的产出:batch 识别结果(回填段落句集,供 live/定稿整流)。
struct SentenceOutcome {
    sentence_id: SentenceId,
    batch_text: Option<String>,
    #[allow(dead_code)]
    asr_ms: u64,
}

/// 句任务:取句 clip(AudioStore,EOS → 段关闭之间存活)→ 阻塞 recognize → 回传
/// `BatchSentence`(经 turn 通道回主循环 emit,单点)。失败/空文本 = 合法 None
/// (消费端回退流式)。
async fn sentence_task(
    s1: Arc<OnnxStage1Recognizer>,
    paragraph_id: ParagraphId,
    sentence_id: SentenceId,
    audio_id: crate::AudioId,
    sr: u32,
    turn: TurnTx,
) -> SentenceOutcome {
    let out = tokio::task::spawn_blocking(move || {
        let pcm = s1.audio_store().concat(&[audio_id]);
        let t0 = Instant::now();
        let text = s1.recognize_once(&pcm, sr, "句级", paragraph_id);
        (text, t0.elapsed().as_millis() as u64)
    })
    .await
    .unwrap_or((None, 0));
    let (text, asr_ms) = out;
    if let Some(text) = &text {
        let _ = turn.send(TurnEvent::BatchSentence {
            paragraph_id,
            sentence_id,
            text: text.clone(),
        });
    }
    SentenceOutcome { sentence_id, batch_text: text, asr_ms }
}

/// 段任务:join 全部句任务(live 联合整流,每句完成一次)→ 段级重跑(多句段落)→
/// 定稿整流一次 → 归档 → `ParagraphCalibration`。全部阻塞调用(LLM/ASR/文件 IO)
/// 都在 `spawn_blocking` 里,任务并发不占事件循环。
async fn paragraph_task(
    paragraph: VadParagraph,
    sr: u32,
    sentence_handles: Vec<tokio::task::JoinHandle<SentenceOutcome>>,
    calibrate: Arc<Mutex<Box<dyn Stage2Calibrator>>>,
    run_paragraph_batch: RunParagraphBatch,
    storage: Option<Arc<Storage>>,
    turn: TurnTx,
) {
    let paragraph_id = paragraph.id;
    let mut acc: Vec<VadSentence> = paragraph.sentences.clone();

    // 就绪门 = join!:每句完成 → 回填 batch → live 联合整流一次(全句 best_text,
    // 严格在该句 BatchSentence 之后 —— BatchSentence 由句任务先行回传)。
    for h in sentence_handles {
        let Ok(out) = h.await else { continue };
        if let Some(s) = acc.iter_mut().find(|s| s.id == out.sentence_id) {
            s.batch_text = out.batch_text;
        }
        let all = acc.clone();
        let cal = Arc::clone(&calibrate);
        let t = Instant::now();
        let calibrated = tokio::task::spawn_blocking(move || cal.lock().unwrap().calibrate_paragraph(paragraph_id, &all))
            .await
            .unwrap_or_default();
        let route_ms = t.elapsed().as_secs_f64() * 1000.0;
        info!(paragraph_id, route_ms = route_ms.round() as u64, calibrated = %calibrated, "纠偏[sentence]");
        let _ = turn.send(TurnEvent::SentenceCalibration { paragraph_id, calibrated, route_ms });
    }

    // 段级重跑(权威 raw):多句段落才跑(单句的拼接 PCM 与句级完全相同,复用句级 batch)。
    let mut paragraph = paragraph;
    if paragraph.sentences.len() > 1 {
        let pcm = Arc::clone(&paragraph.pcm);
        let batch_text = run_paragraph_batch(&pcm, sr, paragraph_id);
        paragraph.batch_asr_ms = 0; // 计时在闭包外拿不到 —— 保留事件字段语义,见下
        if let Some(text) = batch_text {
            paragraph.batch_text = Some(text.clone());
            let _ = turn.send(TurnEvent::BatchParagraph { paragraph_id, text });
        }
    }

    // 定稿整流:一次 LLM(全句 best_text —— 句级 batch 已全部回填,live 整流给不了的)。
    let t = Instant::now();
    let calibrated = {
        let cal = Arc::clone(&calibrate);
        let para = paragraph.clone();
        tokio::task::spawn_blocking(move || cal.lock().unwrap().finalize_paragraph(&para))
            .await
            .unwrap_or_default()
    };
    let route_ms = t.elapsed().as_secs_f64() * 1000.0;
    info!(
        paragraph_id = paragraph.id,
        at_s = (paragraph.start_s * 10.0).round() / 10.0,
        sentences = paragraph.sentences.len(),
        batch = %paragraph.batch_text.clone().unwrap_or_default(),
        streaming = %paragraph.streaming_text,
        calibrated = %calibrated,
        "纠偏[paragraph]"
    );
    // 归档:段落 PCM → audio archive,三份文本 → day log + ring。
    if let Some(storage) = &storage {
        storage.record_final(FinalTurn {
            paragraph_id: paragraph.id,
            at_s: paragraph.start_s,
            duration_ms: paragraph.duration_ms(),
            raw_text: paragraph.best_text().into_owned(),
            streaming_text: paragraph.streaming_text.clone(),
            calibrated: calibrated.clone(),
            route_ms,
            pcm: (*paragraph.pcm).clone(),
        });
    }
    let _ = turn.send(TurnEvent::ParagraphCalibration { paragraph_id, calibrated, route_ms });
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

    /// Counting LLM stub — 断言 live 整流(每句一次)与定稿整流(一次)的调用次数。
    struct CountingLlm(Arc<Mutex<usize>>);
    impl dp_models::LlmProvider for CountingLlm {
        fn complete(&self, _system: &str, _user: &str) -> anyhow::Result<String> {
            *self.0.lock().unwrap() += 1;
            Ok("整流OK".into())
        }
    }

    fn tsent(id: u64, batch: Option<&str>) -> VadSentence {
        VadSentence {
            id,
            audio_id: id,
            start_s: 0.0,
            end_s: 0.5,
            streaming_text: format!("流式{id}"),
            batch_text: batch.map(|b| b.to_string()),
        }
    }

    fn tpar(id: u64, sentences: Vec<VadSentence>) -> VadParagraph {
        VadParagraph {
            id,
            sentences,
            start_s: 0.0,
            end_s: 1.0,
            streaming_text: "流式1流式2".into(),
            batch_text: None,
            pcm: Arc::new(Vec::new()),
            batch_asr_ms: 0,
        }
    }

    // ── round12:paragraph_task(异步定稿管线)单测 ─────────────────────────
    // 句任务用"立即就绪"的伪造 handles(batch 识别本身要真模型,不在单测范围);
    // 断言点:事件顺序(每句 BatchSentence → SentenceCalibration;段级重跑 →
    // BatchParagraph → ParagraphCalibration)、LLM 次数(live n + 定稿 1)。

    /// 上面测试的正确收发形态:接收端在任务运行期间持续收 —— 用 block_on 内联收发。
    #[test]
    fn paragraph_task_multi_sentence_order_and_llm_counts() {
        let calls = Arc::new(Mutex::new(0));
        let llm = Arc::new(CountingLlm(Arc::clone(&calls)));
        let calibrator: Arc<Mutex<Box<dyn Stage2Calibrator>>> = Arc::new(Mutex::new(Box::new(
            Stage2CalibratorImpl::new(llm, Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())), LlmInput::Batch),
        )));
        let (turn, mut rx) = tokio::sync::mpsc::unbounded_channel::<TurnEvent<'static>>();
        let para = tpar(1, vec![tsent(1, None), tsent(2, None)]);
        let run_batch: RunParagraphBatch = Arc::new(|_pcm, _sr, _pid| Some("整段批式".into()));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        // 收 6 个事件:2×(BatchSentence+SentenceCalibration)+BatchParagraph+ParagraphCalibration。
        let mut events = Vec::new();
        rt.block_on(async {
            // 伪造句任务(真实 recognize 需 ONNX 模型,不在单测范围),但模拟真实
            // sentence_task 的行为:先回传 BatchSentence,再交出 outcome。
            let fake = |turn: tokio::sync::mpsc::UnboundedSender<TurnEvent<'static>>,
                        sid: u64,
                        text: &'static str| {
                let t = turn.clone();
                async move {
                    let _ = t.send(TurnEvent::BatchSentence {
                        paragraph_id: 1,
                        sentence_id: sid,
                        text: text.into(),
                    });
                    SentenceOutcome { sentence_id: sid, batch_text: Some(text.into()), asr_ms: 3 }
                }
            };
            let handles = vec![
                tokio::spawn(fake(turn.clone(), 1, "句1批式")),
                tokio::spawn(fake(turn.clone(), 2, "句2批式")),
            ];
            let producer = tokio::spawn(paragraph_task(para, 16000, handles, calibrator, run_batch, None, turn.clone()));
            producer.await.unwrap();
            drop(turn); // 任务已结束、原型关闭 → recv 在收尽后返回 None。
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
        });
        use TurnEvent::*;
        assert_eq!(events.len(), 6, "2×(BatchSentence+SentenceCalibration)+BatchParagraph+ParagraphCalibration");
        assert_eq!(events.iter().filter(|e| matches!(e, BatchSentence { .. })).count(), 2);
        assert_eq!(events.iter().filter(|e| matches!(e, SentenceCalibration { .. })).count(), 2);
        // 顺序不变式:每句 calibration 在其 batch 之后;BatchParagraph 在 ParagraphCalibration 前。
        let mut seen_batch = std::collections::HashSet::new();
        let mut seen_para_batch = false;
        for e in &events {
            match e {
                BatchSentence { sentence_id, .. } => { seen_batch.insert(*sentence_id); }
                SentenceCalibration { .. } => assert!(!seen_batch.is_empty(), "calibration 严格在其 batch 之后(至少该句 batch 已到)"),
                BatchParagraph { .. } => { seen_para_batch = true; assert_eq!(seen_batch.len(), 2, "重跑在全部句 batch 之后"); }
                ParagraphCalibration { .. } => assert!(seen_para_batch, "定稿在整段重跑之后"),
                _ => {}
            }
        }
        assert_eq!(*calls.lock().unwrap(), 3, "live 整流 2 + 定稿 1");
    }

    /// 单句段落:无段级重跑(无 BatchParagraph);live 1 + 定稿 1。
    #[test]
    fn paragraph_task_single_sentence_skips_rerun() {
        let calls = Arc::new(Mutex::new(0));
        let llm = Arc::new(CountingLlm(Arc::clone(&calls)));
        let calibrator: Arc<Mutex<Box<dyn Stage2Calibrator>>> = Arc::new(Mutex::new(Box::new(
            Stage2CalibratorImpl::new(llm, Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())), LlmInput::Batch),
        )));
        let (turn, mut rx) = tokio::sync::mpsc::unbounded_channel::<TurnEvent<'static>>();
        let para = tpar(1, vec![tsent(1, None)]);
        let run_batch: RunParagraphBatch = Arc::new(|_pcm, _sr, _pid| panic!("单句段落不得触发段级重跑"));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let handles = vec![{
                let t = turn.clone();
                tokio::spawn(async move {
                    let _ = t.send(TurnEvent::BatchSentence { paragraph_id: 1, sentence_id: 1, text: "句1批式".into() });
                    SentenceOutcome { sentence_id: 1, batch_text: Some("句1批式".into()), asr_ms: 3 }
                })
            }];
            let producer = tokio::spawn(paragraph_task(para, 16000, handles, calibrator, run_batch, None, turn.clone()));
            producer.await.unwrap();
            drop(turn);
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            use TurnEvent::*;
            assert!(matches!(events.as_slice(),
                [BatchSentence { .. }, SentenceCalibration { .. }, ParagraphCalibration { .. }]),
                "单句段落无 BatchParagraph: {events:?}");
        });
        assert_eq!(*calls.lock().unwrap(), 2, "live 1 + 定稿 1");
    }

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
            Arc::new(AtomicBool::new(false)),
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
            sentences,
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
