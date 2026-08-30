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
//! **本 crate 的线程全部由 `Pipeline::run` 创建**(stage 模块自身不 spawn —— 它们只暴露阻塞
//! 函数)。run() 起三条工作线程 + 本线程跑消费循环:
//!
//! | 线程 | 运行什么(阻塞函数) | 职责 |
//! |---|---|---|
//! | `aura-stage1-ingest` | `s1.run_ingest()` | scout TCP → AudioRing(自动重连) |
//! | `aura-batch` | `s1.run_batch_worker(batch_rx, …)` | 逐个跑**阻塞** batch ASR(句级/段级重跑),每 job 必出一次结果 → `SentenceBatchReady` / `ParagraphBatchReady` |
//! | `aura-stage2` | `Finalizer` 事件循环 | 累积句 batch + **就绪定稿**(全部句级 batch 齐 + 重跑齐 → LLM 整流一次 → 归档 + 定稿事件);live 联合整流(每 `Batch` 一次) |
//! | (调用线程) | `s1.run(...)` | Stage1 消费循环:VAD + 流式 + 边界决策,**永不被 batch 阻塞**(EOS/settle 只入队 job) |
//!
//! Stage2 runs on its own worker so the LLM never blocks the consume loop — streaming partials
//! keep flowing while a paragraph is being calibrated; a `StreamFragment` for sentence N+1 can
//! arrive BEFORE the `ParagraphCalibration` for paragraph N. The worker drains the Stage1
//! triggers off an mpsc channel (`StreamFragment` never crosses it — it passes straight through
//! on the consume-loop thread): `Batch` → [`Stage2Calibrator::calibrate_paragraph`] (live joint
//! calibration) → [`TurnEvent::SentenceCalibration`]; `SentenceBatchReady` → 累积句 batch →
//! [`TurnEvent::BatchSentence`]; `ParagraphEdge` → 开启就绪表;`ParagraphBatchReady` / 全部
//! `SentenceBatchReady` 到齐 → [`Stage2Calibrator::finalize_paragraph`] (ONE LLM pass over the
//! final best texts) → [`TurnEvent::BatchParagraph`] + 归档 +
//! [`TurnEvent::ParagraphCalibration`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info};

use crate::calibrator::{LlmInput, PassThroughCalibrator, Stage2Calibrator, Stage2CalibratorImpl};
use crate::hub::{FinalTurn, Storage};
use crate::recognizer::{BatchJob, BatchJobResult, OnnxStage1Recognizer, Stage1Config, Stage1Recognizer};
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
    s2: Box<dyn Stage2Calibrator>,
    /// Some → run() 定稿时自动 `record_final`(PCM→archive,三份文本→day log+ring)。
    storage: Option<Arc<Storage>>,
    /// Batch-job channel 的接收端(发送端在 `s1` 里):run() spawn 的 `aura-batch` 线程拿它跑
    /// `s1.run_batch_worker`。
    batch_rx: mpsc::Receiver<BatchJob>,
}

impl Pipeline {
    /// Compose an already-built Stage1 + Stage2 (no storage recording). 低层入口 ——
    /// [`Self::assemble`] 是带选型拼装的高层入口;示例(bench)用这个。`batch_rx` 必须来自
    /// `s1` 的构造(`OnnxStage1Recognizer::new` 返回的接收端)。
    pub fn new(
        s1: OnnxStage1Recognizer,
        s2: Box<dyn Stage2Calibrator>,
        batch_rx: mpsc::Receiver<BatchJob>,
    ) -> Self {
        Self { s1, s2, storage: None, batch_rx }
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
        // (s1, batch_rx):batch job 通道的两端 —— sender 在 s1(消费循环入队),receiver 由
        // run() 交给它 spawn 的 aura-batch 线程。
        let (s1, batch_rx) = OnnxStage1Recognizer::new(stage1_config(spec, active, running, flush_paragraph)?)?;
        let s2 = stage2_calibrator(spec, hotwords, corrections)?;
        Ok(Self { s1, s2, storage, batch_rx })
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
        let Pipeline { s1, s2, storage, batch_rx } = self;
        let on_turn = Arc::new(on_turn);
        // 三个线程都要用 s1 —— 共享一份 Arc(本 crate 的 stage 模块不 spawn 线程,全部在这里)。
        let s1 = Arc::new(s1);
        let (tx, rx) = mpsc::channel::<Stage1Event>();

        // ── aura-stage2:Stage2 worker(阻塞 LLM + 就绪定稿;线程归 Pipeline)──────────────
        {
            let on_turn = Arc::clone(&on_turn);
            thread::Builder::new()
                .name("aura-stage2".into())
                .spawn(move || {
                    // Finalizer 单线程独占(无锁):事件在这条线上有序处理,累积/就绪状态
                    // 不可能失步。
                    let mut fin = Finalizer::new(s2, storage);
                    for ev in rx {
                        for out in fin.handle(ev) {
                            on_turn(out);
                        }
                    }
                })
                .expect("spawn aura-stage2 worker");
        }

        // ── aura-batch:batch worker(跑 Stage1 暴露的阻塞 batch 函数;线程归 Pipeline)──
        {
            let s1 = Arc::clone(&s1);
            let tx = tx.clone();
            thread::Builder::new()
                .name("aura-batch".into())
                .spawn(move || {
                    // 每 job 必出一次结果(失败 = None)→ 转成 Stage1 事件汇入 Stage2 通道,
                    // 由 Finalizer 累积并触发就绪定稿。
                    let mut on_result = |res: BatchJobResult| {
                        let ev = match res {
                            BatchJobResult::Sentence {
                                paragraph_id,
                                sentence_id,
                                text,
                                asr_ms,
                            } => Stage1Event::SentenceBatchReady {
                                paragraph_id,
                                sentence_id,
                                batch_text: text,
                                batch_asr_ms: asr_ms,
                            },
                            BatchJobResult::Paragraph { paragraph_id, text, asr_ms } => {
                                Stage1Event::ParagraphBatchReady {
                                    paragraph_id,
                                    batch_text: text,
                                    batch_asr_ms: asr_ms,
                                }
                            }
                        };
                        if tx.send(ev).is_err() {
                            tracing::error!("stage2 worker gone — dropping batch result");
                        }
                    };
                    s1.run_batch_worker(batch_rx, &mut on_result);
                })
                .expect("spawn aura-batch worker");
        }

        // ── aura-stage1-ingest:scout → ring(跑 Stage1 暴露的阻塞 ingest;线程归 Pipeline)─
        {
            let s1 = Arc::clone(&s1);
            thread::Builder::new()
                .name("aura-stage1-ingest".into())
                .spawn(move || s1.run_ingest())
                .expect("spawn aura-stage1-ingest");
        }

        // Stage1 consume loop (this thread) — StreamFragment partials pass straight through;
        // Batch/ParagraphEdge go to the Stage2 worker. The consume loop enqueues batch jobs
        // (never blocks on them) and never blocks on the LLM either.
        // idle 深度睡眠: running=false 时 run() 返回; 等 daemon 恢复(running=true + notify)后重跑。
        loop {
            let s1 = Arc::clone(&s1);
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
                // SentenceBatchReady / ParagraphBatchReady 只来自 batch worker 线程(直接进
                // Stage2 通道)—— 防御性 no-op,万一将来有人从消费循环发它们。
                Stage1Event::SentenceBatchReady { .. } | Stage1Event::ParagraphBatchReady { .. } => {}
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

// ── Finalizer:Stage2 worker 的段落累积 + 就绪定稿状态机(单线程独占,无锁)──────────────
// batch 异步化后,段落定稿要等"全部句级 batch 到齐 + 段级重跑到齐"(单句段落免重跑)。
// 事件在这条 worker 线上有序到达,状态不可能失步:
//   Batch            → 累积句集(保留已到的句级 batch) + live 联合整流 + 开就绪表
//   SentenceBatchReady → 回填句级 batch → BatchSentence + ready += 1
//   ParagraphEdge    → 填入段落与 expected(单句段落 para_done 立即置位 —— 它没有重跑 job)
//   ParagraphBatchReady → 段级重跑结果 → para_done
//   就绪(para_done && ready == expected) → LLM 整流一次(全句 best_text)→
//   BatchParagraph + record_final + ParagraphCalibration。
// 顺序不变式:定稿必在全部 BatchSentence 之后(就绪计数保证);BatchParagraph 允许与部分
// BatchSentence 交错(前端按 id 折叠,voice_state.rs 已鲁棒)。

struct PendingFinal {
    /// 段落本体(ParagraphEdge 填入;`sentences` 定稿时替换为累积的句级 batch 补齐版)。
    paragraph: Option<VadParagraph>,
    /// 句总数(= 句级 batch job 总数);ParagraphEdge 前为 None。
    expected: Option<usize>,
    /// 已收到的 SentenceBatchReady 数(每句恰好一次 —— batch worker 每 job 必出结果)。
    ready: usize,
    /// 段级重跑结果已到;单句段落(无重跑 job)在 ParagraphEdge 即置位。
    para_done: bool,
}

struct Finalizer {
    s2: Box<dyn Stage2Calibrator>,
    storage: Option<Arc<Storage>>,
    /// 每段累积的句集(Batch 带全量快照;SentenceBatchReady 回填句级 batch)。
    sentences: HashMap<ParagraphId, Vec<VadSentence>>,
    /// 每段的定稿就绪表(首个 Batch 开启,定稿时移除)。
    pending: HashMap<ParagraphId, PendingFinal>,
}

impl Finalizer {
    fn new(s2: Box<dyn Stage2Calibrator>, storage: Option<Arc<Storage>>) -> Self {
        Self { s2, storage, sentences: HashMap::new(), pending: HashMap::new() }
    }

    /// 处理一个 Stage1 事件,返回要发出的 TurnEvent(0..=2 个)。
    fn handle(&mut self, ev: Stage1Event) -> Vec<TurnEvent<'static>> {
        match ev {
            Stage1Event::Batch { paragraph_id, sentences } => self.on_batch(paragraph_id, sentences),
            Stage1Event::SentenceBatchReady {
                paragraph_id,
                sentence_id,
                batch_text,
                batch_asr_ms,
            } => self.on_sentence_batch_ready(paragraph_id, sentence_id, batch_text, batch_asr_ms),
            Stage1Event::ParagraphEdge { paragraph } => {
                self.on_paragraph_edge(paragraph);
                Vec::new()
            }
            Stage1Event::ParagraphBatchReady {
                paragraph_id,
                batch_text,
                batch_asr_ms,
            } => self.on_paragraph_batch_ready(paragraph_id, batch_text, batch_asr_ms),
            // StreamFragment 从不过这条通道(消费循环 inline 直发)——防御性 no-op。
            Stage1Event::StreamFragment { .. } => Vec::new(),
        }
    }

    /// Batch(每句 EOS):累积句集(保留已到的句级 batch——SentenceBatchReady 可能已抢先回填)
    /// → 联合 LLM 整流(live 预览,每句一次)。
    fn on_batch(&mut self, paragraph_id: ParagraphId, sentences: Vec<VadSentence>) -> Vec<TurnEvent<'static>> {
        let entry = self.sentences.entry(paragraph_id).or_default();
        for s in sentences {
            if let Some(existing) = entry.iter_mut().find(|x| x.id == s.id) {
                existing.batch_text = existing.batch_text.take().or(s.batch_text);
            } else {
                entry.push(s);
            }
        }
        // 开就绪表(段落首个 Batch)——句级 batch 先于 ParagraphEdge 到达时 ready 即可计数。
        self.pending.entry(paragraph_id).or_insert_with(|| PendingFinal {
            paragraph: None,
            expected: None,
            ready: 0,
            para_done: false,
        });
        let all = entry.clone();
        let t = Instant::now();
        let calibrated = self.s2.calibrate_paragraph(paragraph_id, &all);
        let route_ms = t.elapsed().as_secs_f64() * 1000.0;
        // 联合整流当前段落(每 VAD gap 一次)。
        info!(
            paragraph_id,
            route_ms = route_ms.round() as u64,
            calibrated = %calibrated,
            "纠偏[sentence]"
        );
        vec![TurnEvent::SentenceCalibration { paragraph_id, calibrated, route_ms }]
    }

    /// SentenceBatchReady:回填句级 batch → 发 `BatchSentence`(有文本时)→ 推进就绪计数。
    /// 陈旧事件(段落已定稿、状态已清,或从未见过的段落)→ 忽略,无副作用。
    fn on_sentence_batch_ready(
        &mut self,
        paragraph_id: ParagraphId,
        sentence_id: SentenceId,
        batch_text: Option<String>,
        batch_asr_ms: u64,
    ) -> Vec<TurnEvent<'static>> {
        if !self.sentences.contains_key(&paragraph_id) && !self.pending.contains_key(&paragraph_id) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(entry) = self.sentences.get_mut(&paragraph_id) {
            if let Some(s) = entry.iter_mut().find(|x| x.id == sentence_id) {
                s.batch_text = batch_text.clone();
            }
        }
        if let Some(text) = batch_text {
            out.push(TurnEvent::BatchSentence { paragraph_id, sentence_id, text });
        }
        if let Some(p) = self.pending.get_mut(&paragraph_id) {
            p.ready += 1;
            debug!(
                paragraph_id,
                sentence_id,
                ready = p.ready,
                expected = ?p.expected,
                asr_ms = batch_asr_ms,
                "句级 batch 到达(就绪计数)"
            );
        }
        out.extend(self.try_finalize(paragraph_id));
        out
    }

    /// ParagraphEdge:填入段落与 expected 句数。单句段落没有重跑 job(recognizer 不投递)→
    /// para_done 立即置位,只等句级 batch。
    fn on_paragraph_edge(&mut self, paragraph: VadParagraph) {
        let expected = paragraph.sentences.len();
        let p = self.pending.entry(paragraph.id).or_insert_with(|| PendingFinal {
            paragraph: None,
            expected: None,
            ready: 0,
            para_done: false,
        });
        p.paragraph = Some(paragraph);
        p.expected = Some(expected);
        // 单句段落(无重跑 job)→ 即刻就绪;`||` 保证防御性:若重跑结果已先到(顺序不变式
        // 下不可达)不把它冲掉。
        p.para_done = p.para_done || expected == 1;
    }

    /// ParagraphBatchReady:段级重跑结果(多句段落)→ para_done。
    fn on_paragraph_batch_ready(
        &mut self,
        paragraph_id: ParagraphId,
        batch_text: Option<String>,
        batch_asr_ms: u64,
    ) -> Vec<TurnEvent<'static>> {
        if let Some(p) = self.pending.get_mut(&paragraph_id) {
            if let Some(par) = p.paragraph.as_mut() {
                par.batch_text = batch_text;
                par.batch_asr_ms = batch_asr_ms;
            }
            p.para_done = true;
        }
        self.try_finalize(paragraph_id)
    }

    /// 就绪门:段落已关(expected 已知)+ 段级重跑齐(或单句)+ 全部句级 batch 齐
    /// (ready == expected)→ 定稿:LLM 整流一次(全句 best_text,句级 batch 已补齐)→
    /// BatchParagraph + 归档 + ParagraphCalibration。
    fn try_finalize(&mut self, paragraph_id: ParagraphId) -> Vec<TurnEvent<'static>> {
        let done = match self.pending.get(&paragraph_id) {
            Some(p) => match (&p.paragraph, p.expected) {
                (Some(_), Some(expected)) => p.para_done && p.ready == expected,
                _ => false,
            },
            None => false,
        };
        if !done {
            return Vec::new();
        }
        let Some(p) = self.pending.remove(&paragraph_id) else {
            return Vec::new();
        };
        let mut paragraph = p.paragraph.expect("ready gate passed ⇒ paragraph present");
        // 用累积的句集(句级 batch 已全部回填)替换事件快照 —— 定稿文本用最终 best_text。
        paragraph.sentences = self
            .sentences
            .remove(&paragraph_id)
            .unwrap_or_else(|| paragraph.sentences.clone());
        let t = Instant::now();
        // 定稿整流:一次 LLM(全句 best_text;末句 batch 已齐 —— 这是 live 整流给不了的)。
        let calibrated = self.s2.finalize_paragraph(&paragraph);
        let route_ms = t.elapsed().as_secs_f64() * 1000.0;
        let mut out = Vec::new();
        // BatchParagraph:整段重跑结果(权威 raw_text)。单句段落(无重跑)或重跑失败(None)
        // 不发 —— 前端 plain 预览回退逐句拼接。
        if let Some(text) = paragraph.batch_text.clone() {
            out.push(TurnEvent::BatchParagraph { paragraph_id, text });
        }
        // Log all three text layers — paragraph-level batch (authoritative; empty = re-run
        // failed or single-sentence reuse), the streaming concat, and the Stage2 rewrite — so
        // ASR-level loss is distinguishable from LLM rewriting.
        info!(
            paragraph_id = paragraph.id,
            at_s = (paragraph.start_s * 10.0).round() / 10.0,
            sentences = paragraph.sentences.len(),
            // batch 重跑耗时;单句段落(免重跑)恒 0。
            asr_ms = paragraph.batch_asr_ms,
            route_ms = route_ms.round() as u64,
            batch = %paragraph.batch_text.clone().unwrap_or_default(),
            streaming = %paragraph.streaming_text,
            calibrated = %calibrated,
            "纠偏[paragraph]"
        );
        // 归档:段落 PCM → audio archive,三份文本 → day log + ring(backs /api/audio +
        // /api/recordings)。raw_text 用最终 best_text(重跑缺失时回退句级拼接,不落空串)。
        if let Some(storage) = &self.storage {
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
        out.push(TurnEvent::ParagraphCalibration { paragraph_id, calibrated, route_ms });
        out
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

    // ── Finalizer(就绪定稿)单测 ──────────────────────────────────────────────

    /// Counting LLM stub — 断言 live 整流(每 Batch 一次)与定稿整流(一次)的调用次数。
    struct CountingLlm(Arc<Mutex<usize>>);
    impl dp_models::LlmProvider for CountingLlm {
        fn complete(&self, _system: &str, _user: &str) -> anyhow::Result<String> {
            *self.0.lock().unwrap() += 1;
            Ok("整流OK".into())
        }
    }

    fn fin_with_llm(calls: Arc<Mutex<usize>>) -> Finalizer {
        Finalizer::new(
            Box::new(Stage2CalibratorImpl::new(
                Arc::new(CountingLlm(calls)),
                Arc::new(Mutex::new(Vec::new())),
                Arc::new(Mutex::new(Vec::new())),
                LlmInput::Batch,
            )),
            None,
        )
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

    fn e_batch(pid: u64, sentences: Vec<VadSentence>) -> Stage1Event {
        Stage1Event::Batch { paragraph_id: pid, sentences }
    }
    fn e_sbr(pid: u64, sid: u64, text: &str) -> Stage1Event {
        Stage1Event::SentenceBatchReady {
            paragraph_id: pid,
            sentence_id: sid,
            batch_text: Some(text.into()),
            batch_asr_ms: 0,
        }
    }
    fn e_ped(par: VadParagraph) -> Stage1Event {
        Stage1Event::ParagraphEdge { paragraph: par }
    }
    fn e_pbr(pid: u64, text: &str) -> Stage1Event {
        Stage1Event::ParagraphBatchReady {
            paragraph_id: pid,
            batch_text: Some(text.into()),
            batch_asr_ms: 42,
        }
    }

    /// 多句段落完整流程:定稿必须等"全句 batch 齐 + 重跑齐";定稿 = 1 次 LLM;
    /// BatchParagraph 在 ParagraphCalibration 前;定稿必在所有 BatchSentence 之后。
    #[test]
    fn finalizer_multi_sentence_ready_gate() {
        let calls = Arc::new(Mutex::new(0));
        let mut f = fin_with_llm(Arc::clone(&calls));

        // 句1 EOS:Batch(句1 batch=None)+ live 整流。
        let evs = f.handle(e_batch(1, vec![tsent(1, None)]));
        assert_eq!(evs.len(), 1, "仅 live 校准");
        assert!(!matches!(evs[0], TurnEvent::ParagraphCalibration { .. }));
        assert_eq!(*calls.lock().unwrap(), 1, "每 Batch 一次 live LLM");

        // 句1 batch 先到(段落还没关)—— ready 计数,不定稿。
        let evs = f.handle(e_sbr(1, 1, "句1批式"));
        assert!(matches!(evs[0], TurnEvent::BatchSentence { .. }), "句1 BatchSentence");
        assert_eq!(evs.len(), 1, "未就绪不定稿");

        // 段落关闭(2 句):expected=2,para_done=false(重跑 job 已投递未回)。
        let evs = f.handle(e_ped(tpar(1, vec![tsent(1, None), tsent(2, None)])));
        assert!(evs.is_empty());

        // 句2 EOS:Batch(全句快照,句1 的 batch 已回填)+ live 整流。
        let evs = f.handle(e_batch(1, vec![tsent(1, None), tsent(2, None)]));
        assert_eq!(evs.len(), 1);
        assert_eq!(*calls.lock().unwrap(), 2);

        // 重跑先于句2 batch 完成(池并行)—— 仍不就绪(ready=1 < 2):
        // 保证定稿文本能拿到句2 的 batch,而不是退化成流式。
        let evs = f.handle(e_pbr(1, "整段批式"));
        assert!(evs.is_empty(), "重跑先到但句2 batch 未齐 → 不定稿");

        // 句2 batch 到 → 就绪 → 定稿:BatchParagraph + ParagraphCalibration(定稿 LLM 第 3 次)。
        let evs = f.handle(e_sbr(1, 2, "句2批式"));
        assert_eq!(evs.len(), 3, "BatchSentence + BatchParagraph + ParagraphCalibration");
        assert!(matches!(evs[0], TurnEvent::BatchSentence { .. }));
        assert!(matches!(evs[1], TurnEvent::BatchParagraph { .. }));
        assert!(matches!(evs[2], TurnEvent::ParagraphCalibration { .. }));
        assert_eq!(*calls.lock().unwrap(), 3, "定稿恰好再跑一次 LLM");
        // 定稿后状态清空:再来事件无副作用。
        assert!(f.handle(e_sbr(1, 2, "句2批式")).is_empty());
    }

    /// 单句段落:无重跑 job,ParagraphEdge 即 para_done;只等句级 batch → 定稿;
    /// 无 BatchParagraph(单句复用句级结果,段落 batch_text 恒 None)。
    #[test]
    fn finalizer_single_sentence_reuses_sentence_batch() {
        let calls = Arc::new(Mutex::new(0));
        let mut f = fin_with_llm(Arc::clone(&calls));

        let evs = f.handle(e_batch(1, vec![tsent(1, None)]));
        assert_eq!(evs.len(), 1);
        let evs = f.handle(e_ped(tpar(1, vec![tsent(1, None)])));
        assert!(evs.is_empty(), "单句段落:batch 未到,尚不定稿");
        let evs = f.handle(e_sbr(1, 1, "句1批式"));
        assert_eq!(evs.len(), 2, "BatchSentence + ParagraphCalibration(无 BatchParagraph)");
        assert!(matches!(evs[0], TurnEvent::BatchSentence { .. }));
        assert!(matches!(evs[1], TurnEvent::ParagraphCalibration { .. }));
        assert_eq!(*calls.lock().unwrap(), 2, "live 1 + 定稿 1");
    }

    /// batch 失败(结果 None):不阻塞就绪(每 job 必出结果),定稿按流式回退照常进行。
    #[test]
    fn finalizer_failed_batch_still_finalizes() {
        let calls = Arc::new(Mutex::new(0));
        let mut f = fin_with_llm(Arc::clone(&calls));
        f.handle(e_batch(1, vec![tsent(1, None)]));
        f.handle(e_ped(tpar(1, vec![tsent(1, None)])));
        // 句级 batch 失败(None)—— 无 BatchSentence,但 ready 计数。
        let evs = f.handle(Stage1Event::SentenceBatchReady {
            paragraph_id: 1,
            sentence_id: 1,
            batch_text: None,
            batch_asr_ms: 99,
        });
        assert_eq!(evs.len(), 1, "仅 ParagraphCalibration(batch 失败无 BatchSentence)");
        assert!(matches!(evs[0], TurnEvent::ParagraphCalibration { .. }));
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    /// 顺序不变式:ParagraphCalibration 必在该段所有 BatchSentence 之后
    /// (重跑先完成也不提前定稿 —— 见 finalizer_multi_sentence_ready_gate 的中段)。
    #[test]
    fn finalizer_rerun_before_last_sentence_does_not_finalize_early() {
        let calls = Arc::new(Mutex::new(0));
        let mut f = fin_with_llm(Arc::clone(&calls));
        f.handle(e_batch(1, vec![tsent(1, None)]));
        f.handle(e_sbr(1, 1, "句1批式"));
        f.handle(e_ped(tpar(1, vec![tsent(1, None), tsent(2, None)])));
        f.handle(e_batch(1, vec![tsent(1, None), tsent(2, None)]));
        // 重跑完成,但句2 batch 未回 —— 绝不定稿(否则末句退化成流式)。
        let evs = f.handle(e_pbr(1, "整段批式"));
        assert!(evs.is_empty(), "ready=1 < expected=2 → 不定稿");
        assert_eq!(*calls.lock().unwrap(), 2, "没有为定稿多跑 LLM");
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
