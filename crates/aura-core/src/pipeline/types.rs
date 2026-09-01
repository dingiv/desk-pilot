//! types — pipeline 内部**跨模块纯类型**(数据契约叶子,零逻辑):消息/载荷/回执。
//! 依赖方向铁律:types 只依赖 lib 契约与 vad::Stage0VAD(引擎 trait);其余模块
//! 引用 types,types 不引用其余模块 —— 让执行流文件之间互不借类型。
//!
//! (lib.rs 的对外契约 Stage1Event/TurnEvent/VadSentence/… 仍是 crate 门面,
//! 不在此文件。)

use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::pipeline::vad::Stage0VAD;
use crate::{ParagraphId, SentenceId, VadEvent};

// ── 对外事件词汇(主循环唯一发射点产出)──────────────────────────────────
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


// ── Stage0 → 大脑(拉流线程 → 消费循环)───────────────────────────────────

/// 前端产物(Stage0 → 大脑的队列元素):一帧的检测结果,样本已直发流式(R3)
/// 不再随行。`detected`/`events`/`onset` 同批同序——单 FIFO 天然配对。
pub(crate) struct FrontEvent {
    /// 本帧喂入后的 detected() 快照(大脑:留痕/审计;门控已在前端完成)。
    pub(crate) detected: bool,
    /// 本帧产出的回溯式分句事件(SOS/EOS 同批、EOS 携句 PCM)。
    pub(crate) events: Vec<VadEvent>,
    /// rising-edge 帧(Some)携带起音墙钟 —— 大脑 on_speech_onset + round26
    /// settle 量尺用(与检测同一原点;仅流式配置时置位,与 R2 前行为一致)。
    pub(crate) onset: Option<f64>,
}

/// 前端队列容量:10min @ 32ms/帧 = 18_750。满丢最旧 = 原 AudioRing 环回语义
/// (idle 深睡期间 ingest 持续入队不漏内存)。
pub(crate) const FRONT_Q_CAP: usize = 18_750;

/// 拉流桥的共享句柄集合(ingest_loop 入参打包,免长签名)。`stream_port`:
/// Some = 消费循环在跑(run() 入口安装、退出摘除);None = idle 深睡 —— 帧照喂
/// VAD/入队(等价旧 ring 蓄水),但不进流式。
pub(crate) struct FrontBridge {
    pub(crate) vad: Arc<dyn Stage0VAD>,
    pub(crate) has_stream: bool,
    pub(crate) stream_port: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<StreamCmd>>>>,
    /// 大脑侧 partial 镜像的原子快照(R4 断流喂静音判据;大脑每迭代刷新)。
    pub(crate) partial_live: Arc<AtomicBool>,
    pub(crate) start: std::time::Instant,
    pub(crate) front_q: Arc<Mutex<VecDeque<FrontEvent>>>,
    pub(crate) notify: Arc<Notify>,
    pub(crate) active: Arc<AtomicBool>,
}

// ── 大脑 ⇄ 流式任务(cmd/out 一对通道)────────────────────────────────────

/// 大脑/前端 → 流式任务指令。Feed/Onset 由拉流线程门控直发(R3);
/// Reset/Finalize 由大脑发。
pub(crate) enum StreamCmd {
    /// 起音:重置会话 + 补喂 lead_in(soft onset)。`at` = 起音墙钟(R3 起由前端
    /// 携带,大脑 settle 量尺同源;流式侧仅留痕用)。
    Onset { at: f64, lead_in: Vec<Vec<i16>> },
    /// 语音帧(`detected()` 门控;断流时的合成静音帧同路)。
    Feed(Vec<i16>),
    /// 会话重置(段落边界 / 停滞看门狗)。
    Reset,
    /// EOS 定稿:B 侧 finalize_and_result,回执(`StreamOut::Finalized`)经 out
    /// 通道返回,随后自重置会话。
    Finalize,
}

/// finalize 回执。`pcm: None` = 流式未配置(调用方 fallback VAD 句)。
pub(crate) struct StreamFinal {
    pub(crate) text: String,
    pub(crate) pcm: Option<Vec<i16>>,
    pub(crate) fed: u32,
}

/// 流式任务回传(out 通道):partial 或 EOS 定稿回执 —— 单通道双向语义,不再有
/// per-句 oneshot(round24)。
pub(crate) enum StreamOut {
    /// partial:`text = Some(新 partial)` 仅在非空且变化时(→ 消费循环发射 SF);
    /// `nonempty` = B 侧 last_partial 非空(speaking 抑制镜像)。
    Partial { text: Option<String>, nonempty: bool },
    /// EOS 定稿回执(必为该句最后一个回传消息 —— B 收到 Finalize 后不再有 Feed 在途)。
    Finalized(Box<StreamFinal>),
}

/// B 侧 last_partial 状态的消费循环镜像(speaking 抑制 / 停滞看门狗判据):
/// 每次 B 回传刷新;重置/定稿点由大脑直接清零(确定性,无竞态)。
#[derive(Clone, Copy)]
pub(crate) struct PartialMirror {
    pub(crate) nonempty: bool,
    pub(crate) last_change: std::time::Instant,
}

impl PartialMirror {
    pub(crate) fn empty() -> Self {
        Self { nonempty: false, last_change: std::time::Instant::now() }
    }
}

// ── 边界/任务契约(tracker / batch 任务)─────────────────────────────────

/// A paragraph closed by a big gap or the settle-timeout — the recognizer turns this into a
/// [`crate::VadParagraph`] (concat PCM + paragraph-level batch re-run) and emits `ParagraphEdge`.
pub(crate) struct SettledParagraph {
    pub(crate) paragraph_id: ParagraphId,
    pub(crate) sentences: Vec<crate::VadSentence>,
}

/// 句任务的产出:batch 识别结果(回填段落句集,供 live/定稿整流)。
pub(crate) struct SentenceOutcome {
    pub(crate) sentence_id: SentenceId,
    pub(crate) batch_text: Option<String>,
    #[allow(dead_code)]
    pub(crate) asr_ms: u64,
}

/// 段任务的等待集(就绪门):句任务 handles(回填句 batch)+ live 整流链尾
/// (该段全部 SC 先于 PCal 的契约保证)。
pub(crate) struct ParagraphWaits {
    pub(crate) sentences: Vec<tokio::task::JoinHandle<SentenceOutcome>>,
    pub(crate) live: Option<tokio::task::JoinHandle<()>>,
}

/// 任务 → 主循环的回传通道(发射单点:BS/SC/BP/PCal 全部经它)。
pub(crate) type TurnTx = tokio::sync::mpsc::UnboundedSender<TurnEvent<'static>>;

/// 段级重跑的调用壳(batch 单发入口的闭包形态,段任务持有)。
pub(crate) type RunParagraphBatch =
    Arc<dyn Fn(&[i16], u32, ParagraphId) -> Option<String> + Send + Sync>;
