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
//! | 载体 | 运行什么 | 职责 |
//! |---|---|---|
//! | blocking pool ×1 | `s1.run_ingest()` | scout TCP → AudioRing(自动重连;sync IO)|
//! | 异步任务(消费循环) | `s1.run(cb).await` | VAD + 流式 + 边界决策(**起音即开段**,时间戳 id;帧等待走 Notify;深睡 = 等 resume Notify 重跑);cb 只把 `Stage1Event` 推进 tokio channel(batch_jobs=false,不投 job) |
//! | `run` future(daemon: rt 任务 / 独立宿主: 专用线程) | `select!` 主循环 | **唯一 on_turn 调用者**:StreamFragment/ParagraphClosed 直通 emit;Batch → 句任务(只投 just-closed 句)+ live 整流任务;ParagraphEdge → 段任务 |
//! | 句任务 | `spawn_blocking(recognize_once)` | 每句 EOS 一个;完成即回传 `BatchSentence` |
//! | live 整流任务 | 链式 `spawn_blocking(calibrate_paragraph)` | **每句 batch 完成后一个**(BS 到达触发 —— 架构要求 batch 完成 → 之后纠偏,先后明确;段内链式串行,SC 顺序 = 段落生长序);回传 `SentenceCalibration` |
//! | 段任务 | join 句任务 + live 链尾 → 段重跑 → LLM 定稿 → 归档 | 就绪门 = `join!` 语义;SC 先于 PCal |
//!
//! 时序语义:边界(`ParagraphClosed`)经 s1 通道 FIFO + 主循环按序 emit,必先于下一段
//! 任何事件(结构保证);跨段乱序(段 N 定稿 vs 段 N+1 流式)是物理现实,客户端按
//! `paragraph_id` 修订。旧 Finalizer 就绪门状态机(ready/expected/para_done 计数)
//! 被任务结构取代 —— `run_batch_worker`/`SentenceBatchReady`/`ParagraphBatchReady`
//! 在新编排下不再使用(保留供旧编排/测试)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use tracing::info;

use calibrator::{LlmInput, PassThroughCalibrator, Stage2Calibrator, Stage2CalibratorImpl};

// round23:流水线环节文件集中在 pipeline/ 模块文件夹(mod = 编排,其余 = 环节)。
pub mod calibrator;
pub mod consume;
pub mod vad;
pub mod tasks;
pub mod recognizer;
pub mod stream;
pub mod tracker;
use crate::hub::Storage;
use recognizer::{OnnxStage1Recognizer, Stage1Config};
use tasks::{
    emit_turn, live_calibration_task, paragraph_task, sentence_task, ParagraphWaits,
    RunParagraphBatch, SentenceOutcome,
};
use crate::{ParagraphId, SentenceId, Stage1Event, VadParagraph, VadSentence};
use dp_models::http::HttpLlm;

use tokio;
use tokio::sync::Notify;

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
    /// Stage2's provisional JOINT calibration of the current paragraph — one per sentence
    /// **batch completion** (round17b: STRICTLY after that sentence's `BatchSentence` —
    /// 架构需求"batch 完成 → 之后纠偏,先后明确"). The calibrated text so far, replacing
    /// the previous calibration of the same paragraph.
    SentenceCalibration { paragraph_id: u64, sentence_id: SentenceId, calibrated: String, route_ms: f64 },
    /// The settled paragraph's final calibration — ONE LLM pass over the final best texts
    /// (all sentence batches are in by the readiness gate; the last live calibration may not
    /// have included the final sentence's batch, hence the extra pass). Paragraph-granularity
    /// final (D3).
    ParagraphCalibration { paragraph_id: u64, calibrated: String, route_ms: f64 },
}

// ── select! 两臂的处理器(round24 R4:臂体拆出,循环只剩分派)─────────────────
// 状态账本(三张表)与处理器分离:循环 = 事件分派,处理器 = 各事件的编排语义。

/// select! 处理器的共享依赖(run 存续期不变;round24:收进一个结构,处理器签名不再长参)。
struct Ctx<F> {
    s1: Arc<OnnxStage1Recognizer>,
    s2: Arc<Mutex<Box<dyn Stage2Calibrator>>>,
    storage: Option<Arc<Storage>>,
    turn: tasks::TurnTx,
    on_turn: Arc<F>,
}

/// 主循环的可变编排状态:每段①句任务 handles(Batch 入,ParagraphEdge 移交段任务
/// join)②live 整流链尾(SC 串行序)③已回填句集(Batch 快照为底 + BS patch,
/// round16b —— live SC 的输入带上前句 batch)。
#[derive(Default)]
struct Turns {
    pending: HashMap<ParagraphId, Vec<tokio::task::JoinHandle<SentenceOutcome>>>,
    live_chain: HashMap<ParagraphId, tokio::task::JoinHandle<()>>,
    open_sents: HashMap<ParagraphId, Vec<VadSentence>>,
}

/// s1 臂:`Batch`(句 EOS)—— 只为 just-closed 句(载荷最后一个)投句任务(载荷是
/// 该段全部句快照,为无状态 Stage2 设计;全量重投 = N² ASR/LLM,§7-D 回归已修);
/// 再以 Batch 快照为底**合并** open_sents —— 保留 BS 已回填的句 batch(快照来自
/// tracker 副本,batch_text 恒 None,直接覆写会把 batch 冲掉,SC 输入退回全流式)。
/// live SC 的触发点**不在这里**(round17b:架构要求 batch 完成 → 之后纠偏,触发点在
/// 该句 BS 到达时,见 [`on_turn_batch_sentence`])。
fn on_stage1_batch<F: Fn(TurnEvent)>(
    ctx: &Ctx<F>,
    turns: &mut Turns,
    paragraph_id: ParagraphId,
    sentences: &[VadSentence],
    sr: u32,
) {
    let entry = turns.pending.entry(paragraph_id).or_default();
    if let Some(s) = sentences.last() {
        entry.push(tokio::spawn(sentence_task(
            Arc::clone(&ctx.s1), paragraph_id, s.id, s.audio_id, sr, ctx.turn.clone(),
        )));
    }
    let entry = turns.open_sents.entry(paragraph_id).or_default();
    for s in sentences {
        if let Some(prev) = entry.iter_mut().find(|p| p.id == s.id) {
            let keep = prev.batch_text.clone();
            *prev = s.clone();
            prev.batch_text = keep.or(s.batch_text.clone());
        } else {
            entry.push(s.clone());
        }
    }
}

/// s1 臂:`ParagraphEdge`(段关闭)—— 先 emit `ParagraphClosed`(边界时序 round11 S3:
/// PC 先于下一段任何事件 —— s1 通道 FIFO + 主循环按序 emit,结构保证),再投段任务
/// (join 等待集 + live 链尾 → 段重跑 → 定稿 → 归档)。
fn on_stage1_paragraph_edge<F: Fn(TurnEvent)>(
    ctx: &Ctx<F>,
    turns: &mut Turns,
    paragraph: VadParagraph,
    sr: u32,
) {
    emit_turn(&*ctx.on_turn, TurnEvent::ParagraphClosed { paragraph_id: paragraph.id });
    turns.open_sents.remove(&paragraph.id);
    let waits = ParagraphWaits {
        sentences: turns.pending.remove(&paragraph.id).unwrap_or_default(),
        live: turns.live_chain.remove(&paragraph.id),
    };
    let s1c = Arc::clone(&ctx.s1);
    let run_batch: RunParagraphBatch =
        Arc::new(move |pcm, sr, pid| s1c.recognize_once(pcm, sr, "段落级重跑", pid));
    tokio::spawn(paragraph_task(
        paragraph, sr, waits, Arc::clone(&ctx.s2), run_batch, ctx.storage.clone(),
        ctx.turn.clone(),
    ));
}

/// turn 臂:`BatchSentence` 到达(round17b:架构需求"batch 完成 → 之后纠偏,先后
/// 明确")—— 回填该段句集,段仍开放则链式触发一次联合整流(SC 严格在该句 BS 之后,
/// 输入带上它的 batch)。链式串行 → SC 顺序 = 段落生长顺序。段已关闭(迟到 BS)不
/// 触发 —— PCal 已由段任务回填该 batch,迟到的 SC 会破坏"PCal 是该段最后事件"的契约。
fn on_turn_batch_sentence<F: Fn(TurnEvent)>(
    ctx: &Ctx<F>,
    turns: &mut Turns,
    paragraph_id: ParagraphId,
    sentence_id: SentenceId,
    text: &str,
) {
    if let Some(v) = turns.open_sents.get_mut(&paragraph_id) {
        if let Some(s) = v.iter_mut().find(|s| s.id == sentence_id) {
            s.batch_text = Some(text.to_string());
        }
        let texts = v.clone();
        let prev = turns.live_chain.remove(&paragraph_id);
        turns.live_chain.insert(
            paragraph_id,
            tokio::spawn(live_calibration_task(
                prev, paragraph_id, sentence_id, texts, Arc::clone(&ctx.s2), ctx.turn.clone(),
            )),
        );
    }
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
        // round12 起:batch pass 由 pipeline 的任务结构自建(recognize_once 直调)。
        let cfg = stage1_config(spec, active, running, flush_paragraph)?;
        let s1 = OnnxStage1Recognizer::new(cfg)?;
        let s2 = stage2_calibrator(spec, hotwords, corrections)?;
        Ok(Self { s1, s2: Arc::new(Mutex::new(s2)), storage })
    }

    /// Run the pipeline. **round14:全异步** —— 唯一的阻塞桥(scout TCP ingest,sync IO)
    /// 经 `spawn_blocking` 骑 runtime blocking pool;消费循环(round14b 起本体 async,
    /// 帧等待走 Notify)与 `select!` 主循环都是原生异步任务/future。run() 内部**不声明
    /// 任何 std 线程**。消费循环在 `running` 置 false(idle 深度睡眠)时退出,在
    /// `resume`(Notify)被唤醒(daemon 置 running=true + notify)后重跑。本 future
    /// 永不完成(无限 select 循环)—— 宿主要么 `spawn` 它,要么用 [`Self::spawn`] 便捷
    /// 入口。
    pub async fn run<F>(
        self,
        running: Arc<AtomicBool>,
        resume: Arc<Notify>,
        on_turn: F,
    ) where
        F: Fn(TurnEvent) + Send + Sync + 'static,
    {
        let Pipeline { s1: stage1, s2, storage } = self;
        let on_turn = Arc::new(on_turn);
        // s1 被消费线程、任务(recognize_once)共享 —— Arc。
        let stage1 = Arc::new(stage1);

        // 事件桥:s1 消费循环(阻塞)→ 主循环。unbounded:send 永不阻塞消费循环。
        let (s1_tx, mut s1_rx) = tokio::sync::mpsc::unbounded_channel::<Stage1Event>();
        // 任务产出回传:BatchSentence / SentenceCalibration / BatchParagraph /
        // ParagraphCalibration 全部经它回主循环 —— **on_turn 只被本 future 调用**
        // (发射单点,消除多线程并发回调的竞态)。
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel::<TurnEvent<'static>>();

        // ── 阻塞桥 ①:scout TCP → ring(blocking pool;自动重连,永不返回)──────
        {
            let s1: Arc<OnnxStage1Recognizer> = Arc::clone(&stage1);
            let span = tracing::info_span!("aura-stage1-ingest");
            tokio::task::spawn_blocking(move || {
                let _g = span.enter();
                s1.run_ingest()
            });
        }

        // ── 桥 ②:s1 消费循环 —— **原生异步任务**(round14b:run 本体 async,帧等待走
        //     Notify;VAD 每 32ms 微秒级 + 流式解码 0.3s 节流,内联在 executor 上是
        //     协作式调度的标准负载)。流式/VAD/边界决策;batch_jobs=false 不投 job。
        {
            let s1 = Arc::clone(&stage1);
            let span = tracing::info_span!("aura-stage1");
            tokio::spawn(async move {
                let _g = span.enter();
                loop {
                    let s1_tx = s1_tx.clone();
                    s1.run(&mut move |ev| {
                        if s1_tx.send(ev).is_err() {
                            tracing::error!("pipeline main loop gone — dropping stage1 event");
                        }
                    })
                    .await;
                    // 深度睡眠(异步):running=false → run 返回;daemon 置回 true +
                    // notify 后重跑消费循环(Notify permit 语义,无丢唤醒)。
                    while !running.load(Ordering::Relaxed) {
                        resume.notified().await;
                    }
                }
            });
        }

        // ── 主循环(本 future):select 两源,单点 emit;臂体在上方 on_* 处理器 ──
        let ctx = Ctx { s1: stage1, s2, storage, turn: turn_tx, on_turn };
        let mut turns = Turns::default();
        loop {
            tokio::select! {
                ev = s1_rx.recv() => match ev {
                    Some(Stage1Event::StreamFragment { paragraph_id, sentence_id, text, at_s }) => {
                        // 高频(说话中 ~0.3s/条)—— 直通低延迟路径。
                        emit_turn(&*ctx.on_turn, TurnEvent::StreamFragment {
                            paragraph_id,
                            sentence_id,
                            text: &text,
                            at_s,
                        });
                    }
                    Some(Stage1Event::Batch { paragraph_id, sentences, sr }) => {
                        on_stage1_batch(&ctx, &mut turns, paragraph_id, &sentences, sr);
                    }
                    Some(Stage1Event::ParagraphEdge { paragraph, sr }) => {
                        on_stage1_paragraph_edge(&ctx, &mut turns, paragraph, sr);
                    }
                    None => {} // s1 线程深睡后 channel 仍存活,不会 None;防御。
                },
                ev = turn_rx.recv() => {
                    // None = 全部任务结束才会发生;任务随 select 循环存续,防御。
                    if let Some(t) = ev {
                        if let TurnEvent::BatchSentence { paragraph_id, sentence_id, text } = &t {
                            on_turn_batch_sentence(
                                &ctx, &mut turns, *paragraph_id, *sentence_id, text,
                            );
                        }
                        emit_turn(&*ctx.on_turn, t);
                    }
                }
            }
        }   // loop:本 future 永不完成
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

#[cfg(test)]
mod tests {
    use super::*;
    use tasks::describe_turn;
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
    // 断言点:事件顺序(BatchSentence → … → BatchParagraph → ParagraphCalibration;
    // live SC 由 BS 到达时的 live_calibration_task 发(round17b:turn 臂触发,
    // 不归段任务)。

    /// describe_turn(统一发射留痕)的格式快照 —— 日志序列是前后端时序对表的
    /// 契约,六种事件各一行,格式变更需有意识地改这里。
    #[test]
    fn describe_turn_covers_all_event_kinds() {
        let s = describe_turn(&TurnEvent::StreamFragment {
            paragraph_id: 1756615200123456,
            sentence_id: 2,
            text: "你好",
            at_s: 12.345,
        });
        assert!(s.contains("stream p1756615200123456 s2 @12.35"), "{s}");
        assert!(s.contains(r#""你好""#), "{s}");
        assert_eq!(
            describe_turn(&TurnEvent::ParagraphClosed { paragraph_id: 7 }),
            "paragraph_closed p7"
        );
        assert!(describe_turn(&TurnEvent::BatchSentence {
            paragraph_id: 7, sentence_id: 1, text: "批".into(),
        }).starts_with("batch_sentence p7 s1"));
        assert!(describe_turn(&TurnEvent::BatchParagraph { paragraph_id: 7, text: "整段".into() })
            .starts_with("batch_paragraph p7"));
        assert!(describe_turn(&TurnEvent::SentenceCalibration {
            paragraph_id: 7, sentence_id: 2, calibrated: "整流".into(), route_ms: 850.4,
        }).starts_with("sentence_calibration p7 s2 850ms"));
        assert!(describe_turn(&TurnEvent::ParagraphCalibration {
            paragraph_id: 7, calibrated: "定稿".into(), route_ms: 1234.0,
        }).starts_with("paragraph_calibration p7 1234ms"));
    }

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
        // 收 4 个事件:2×BatchSentence + BatchParagraph + ParagraphCalibration(SC 由
        // BS 到达触发(turn 臂 live 链),不归段任务)。
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
            let producer = tokio::spawn(paragraph_task(para, 16000, ParagraphWaits { sentences: handles, live: None }, calibrator, run_batch, None, turn.clone()));
            producer.await.unwrap();
            drop(turn); // 任务已结束、原型关闭 → recv 在收尽后返回 None。
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
        });
        use TurnEvent::*;
        assert_eq!(events.len(), 4, "2×BatchSentence + BatchParagraph + ParagraphCalibration");
        assert_eq!(events.iter().filter(|e| matches!(e, BatchSentence { .. })).count(), 2);
        // 顺序不变式:BatchParagraph 在全部句 batch 之后、ParagraphCalibration 之前。
        let mut seen_batch = std::collections::HashSet::new();
        let mut seen_para_batch = false;
        for e in &events {
            match e {
                BatchSentence { sentence_id, .. } => { seen_batch.insert(*sentence_id); }
                BatchParagraph { .. } => { seen_para_batch = true; assert_eq!(seen_batch.len(), 2, "重跑在全部句 batch 之后"); }
                ParagraphCalibration { .. } => assert!(seen_para_batch, "定稿在整段重跑之后"),
                _ => {}
            }
        }
        assert_eq!(*calls.lock().unwrap(), 1, "段任务只跑定稿整流 1 次(live 在 turn 臂)");
    }

    /// 单句段落:无段级重跑(无 BatchParagraph);定稿 1 次 LLM。
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
            let producer = tokio::spawn(paragraph_task(para, 16000, ParagraphWaits { sentences: handles, live: None }, calibrator, run_batch, None, turn.clone()));
            producer.await.unwrap();
            drop(turn);
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            use TurnEvent::*;
            assert!(matches!(events.as_slice(),
                [BatchSentence { .. }, ParagraphCalibration { .. }]),
                "单句段落无 BatchParagraph: {events:?}");
        });
        assert_eq!(*calls.lock().unwrap(), 1, "定稿 1 次");
    }

    /// **回归(round16b,实测日志钉死)**:单句段落 + PassThrough(LLM disabled)
    /// —— BS 比 PC 早到 3.4s,但定稿输入的 batch 是空的(ParagraphEdge 快照未回填)
    /// → PCal 发出**流式**文本,把 finals 的 batch 占位换回去。修复后:join 回填
    /// 写回段落实体,PCal = 句 batch(而非快照里的流式)。
    #[test]
    fn paragraph_task_finalize_uses_backfilled_sentence_batches() {
        let calibrator: Arc<Mutex<Box<dyn Stage2Calibrator>>> =
            Arc::new(Mutex::new(Box::new(PassThroughCalibrator)));
        let (turn, mut rx) = tokio::sync::mpsc::unbounded_channel::<TurnEvent<'static>>();
        // 快照:句 1 只有流式文本(batch_text: None —— ParagraphEdge 时刻的真实形态)。
        let para = tpar(1, vec![tsent(1, None)]);
        let run_batch: RunParagraphBatch = Arc::new(|_pcm, _sr, _pid| panic!("单句段落不得触发段级重跑"));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let handles = vec![{
                let t = turn.clone();
                tokio::spawn(async move {
                    let _ = t.send(TurnEvent::BatchSentence {
                        paragraph_id: 1,
                        sentence_id: 1,
                        text: "bug太多啦，太多啦！".into(),   // ← BS(batch,带标点)
                    });
                    // 句任务回填的结果 —— join 后必须进入定稿输入。
                    SentenceOutcome { sentence_id: 1, batch_text: Some("bug太多啦，太多啦！".into()), asr_ms: 70 }
                })
            }];
            let producer = tokio::spawn(paragraph_task(
                para, 16000, ParagraphWaits { sentences: handles, live: None },
                calibrator, run_batch, None, turn.clone(),
            ));
            producer.await.unwrap();
            drop(turn);
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            use TurnEvent::*;
            assert!(matches!(events.as_slice(),
                [BatchSentence { .. }, ParagraphCalibration { calibrated, .. }]
                    if calibrated == "bug太多啦，太多啦！"),
                "PCal 必须用回填后的句 batch(实测曾发出流式 \"流式1\"): {events:?}");
        });
    }

    /// **回归(round17,实测 panic)**:多句段落的重跑闭包 panic(实测:HttpAsr 的
    /// reqwest::blocking 在 async 上下文 drop runtime 崩)—— 段任务不得死,BP 跳过、
    /// **PCal 必发**(回退句级 batch);归档照常路径不崩。
    #[test]
    fn paragraph_task_survives_rerun_panic_and_still_finalizes() {
        let calibrator: Arc<Mutex<Box<dyn Stage2Calibrator>>> =
            Arc::new(Mutex::new(Box::new(PassThroughCalibrator)));
        let (turn, mut rx) = tokio::sync::mpsc::unbounded_channel::<TurnEvent<'static>>();
        let para = tpar(1, vec![tsent(1, None), tsent(2, None)]);
        let run_batch: RunParagraphBatch = Arc::new(|_pcm, _sr, _pid| panic!("重跑崩了(实测形态)"));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let handles = vec![
                tokio::spawn(async {
                    SentenceOutcome { sentence_id: 1, batch_text: Some("句1批".into()), asr_ms: 1 }
                }),
                tokio::spawn(async {
                    SentenceOutcome { sentence_id: 2, batch_text: Some("句2批".into()), asr_ms: 1 }
                }),
            ];
            let producer = tokio::spawn(paragraph_task(
                para, 16000, ParagraphWaits { sentences: handles, live: None },
                calibrator, run_batch, None, turn.clone(),
            ));
            producer.await.unwrap();
            drop(turn);
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            use TurnEvent::*;
            // 无 BP(重跑崩);PCal 必发,且用回填的句 batch(拼接)。
            assert!(matches!(events.as_slice(),
                [ParagraphCalibration { calibrated, .. }] if calibrated == "句1批句2批"),
                "重跑 panic → PCal 仍必发(句 batch 回退): {events:?}");
        });
    }

    /// live 整流链(架构需求:batch 完成 → 之后纠偏):段内两句的 batch 先后完成 →
    /// 两条 SC **按 Batch 顺序**串行(链式 await),LLM 恰好 2 次。定长 LLM 返回
    /// 每次调用不同的文本,断言顺序即断言链。
    #[test]
    fn live_calibration_task_chains_per_batch_order() {
        struct SeqLlm(Arc<Mutex<usize>>);
        impl dp_models::LlmProvider for SeqLlm {
            fn complete(&self, _system: &str, _user: &str) -> anyhow::Result<String> {
                let mut n = self.0.lock().unwrap();
                *n += 1;
                Ok(format!("整流{n}"))
            }
        }
        let calls = Arc::new(Mutex::new(0));
        let llm = Arc::new(SeqLlm(Arc::clone(&calls)));
        let calibrator: Arc<Mutex<Box<dyn Stage2Calibrator>>> = Arc::new(Mutex::new(Box::new(
            Stage2CalibratorImpl::new(llm, Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())), LlmInput::Batch),
        )));
        let (turn, mut rx) = tokio::sync::mpsc::unbounded_channel::<TurnEvent<'static>>();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // Batch#1(1 句)→ live1;Batch#2(2 句)→ live2 链在 live1 后。
            let h1 = tokio::spawn(live_calibration_task(
                None, 7, 1, vec![tsent(1, None)], Arc::clone(&calibrator), turn.clone(),
            ));
            let h2 = tokio::spawn(live_calibration_task(
                Some(h1), 7, 2, vec![tsent(1, None), tsent(2, None)], Arc::clone(&calibrator), turn.clone(),
            ));
            h2.await.unwrap();
            drop(turn);
            let mut scs = Vec::new();
            while let Some(ev) = rx.recv().await {
                if let TurnEvent::SentenceCalibration { calibrated, .. } = ev {
                    scs.push(calibrated);
                }
            }
            // 链式保序:SC#1(整流1)先于 SC#2(整流2)—— 即使两个任务并发 spawn。
            assert_eq!(scs, vec!["整流1".to_string(), "整流2".to_string()], "SC 按 Batch 顺序串行");
        });
        assert_eq!(*calls.lock().unwrap(), 2, "每 Batch 恰一次 live LLM");
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
        assert!(matches!(cfg.asr_kind, ProviderKind::Local), "不影响 streaming/VAD 的本地路径");

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
