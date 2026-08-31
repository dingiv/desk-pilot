//! tasks — 流水线的任务壳(round23 从 mod.rs 拆出):
//!   [`sentence_task`] 句级 batch(每句 EOS 触发,spawn_blocking recognize)→
//!   BatchSentence;[`live_calibration_task`] live 纠偏(BS 到达触发,段内链式串行)→
//!   SentenceCalibration;[`paragraph_task`] 段定稿(join 句任务 + 段重跑 + 定稿整流 +
//!   归档)→ BatchParagraph / ParagraphCalibration。
//! 触发与汇点(select! 单点 emit)在 mod.rs;资源(recognize_once/AudioStore)在
//! recognizer.rs。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::hub::{FinalTurn, Storage};
use crate::pipeline::calibrator::Stage2Calibrator;
use crate::pipeline::recognizer::OnnxStage1Recognizer;
use crate::{ParagraphId, SentenceId, TurnEvent, VadParagraph, VadSentence};


// ── round12 异步化:per-sentence / per-paragraph 任务(替代 Finalizer 状态机)────────
//
// 编排模型(与 ime-core IoThread 同构):s1 消费循环(阻塞线程)产 Stage1Event →
// tokio channel;主循环 select! 单点 emit;batch 由任务自建(spawn_blocking 直调
// `recognize_once`)。时序不变式不再靠手写计数门,而是任务结构本身:
//
//   句任务(Batch/EOS 触发,只投 just-closed 句)→ 完成即回传 BatchSentence;
//   live 整流任务(BS 到达触发 = 该句 batch 完成后,段内链式串行)→
//   SentenceCalibration——架构需求
//   "1s 空白 → Batch 识别,stage2 紧跟纠偏,先后明确"在段开放期间持续发生;
//   段任务(ParagraphEdge 触发)→ join 全部句任务(就绪门)+ live 链尾 →
//   段级重跑(多句;单句复用句级)→ BatchParagraph → 定稿整流一次 → 归档 +
//   ParagraphCalibration。live 链尾在定稿前收束 → 该段全部 SC 先于 PCal(契约)。
//
// 跨段乱序(段 N 定稿 vs 段 N+1 流式)是物理现实 —— 客户端按 paragraph_id 修订。

/// 段级重跑闭包:_blocking recognize(生产 = `s1.recognize_once`;测试注入 stub)。
pub(crate) type RunParagraphBatch = Arc<dyn Fn(&[i16], u32, ParagraphId) -> Option<String> + Send + Sync>;

/// 单行事件摘要(统一发射留痕用)。段落 id 是时间戳微秒 —— 日志里的 p 值本身即
/// 段落创建时刻,时序对表时直接可比大小。
pub(crate) fn describe_turn(ev: &TurnEvent) -> String {
    match ev {
        TurnEvent::StreamFragment { paragraph_id, sentence_id, text, at_s } => {
            format!("stream p{paragraph_id} s{sentence_id} @{at_s:.2} {text:?}")
        }
        TurnEvent::ParagraphClosed { paragraph_id } => {
            format!("paragraph_closed p{paragraph_id}")
        }
        TurnEvent::BatchSentence { paragraph_id, sentence_id, text } => {
            format!("batch_sentence p{paragraph_id} s{sentence_id} {text:?}")
        }
        TurnEvent::BatchParagraph { paragraph_id, text } => {
            format!("batch_paragraph p{paragraph_id} {text:?}")
        }
        TurnEvent::SentenceCalibration { paragraph_id, sentence_id, calibrated, route_ms } => {
            format!("sentence_calibration p{paragraph_id} s{sentence_id} {route_ms:.0}ms {calibrated:?}")
        }
        TurnEvent::ParagraphCalibration { paragraph_id, calibrated, route_ms } => {
            format!("paragraph_calibration p{paragraph_id} {route_ms:.0}ms {calibrated:?}")
        }
    }
}

/// **统一发射留痕(round16 调试)**:主循环是 on_turn 的唯一调用者(单点发射),
/// 每一条即将发往前端的事件先记一条 info 再发 —— 前后端时序错位排查时,以这条
/// 序列为权威对表(server 侧实际发出的顺序/内容)。
pub(crate) fn emit_turn<F>(on_turn: &F, ev: TurnEvent<'_>)
where
    F: Fn(TurnEvent),
{
    // 级别(round25):流式高频 → debug;batch/纠偏/边界 → info。
    match ev {
        TurnEvent::StreamFragment { .. } => debug!(event = %describe_turn(&ev), "emit→前端"),
        _ => info!(event = %describe_turn(&ev), "emit→前端"),
    }
    on_turn(ev);
}
/// 任务产出回传通道(pipeline 内部;主循环 drain → 单点 emit)。
/// 墙钟时刻(HH:MM:SS.mmm,本地时区)—— batch/纠偏调用的起止对表用(round25)。
fn wall(t: chrono::DateTime<chrono::Local>) -> String {
    t.format("%H:%M:%S%.3f").to_string()
}

pub(crate) type TurnTx = tokio::sync::mpsc::UnboundedSender<TurnEvent<'static>>;
/// 段级重跑的兜底超时:HttpAsr 自带请求级超时,这里保证"重跑无论挂死/panic,
/// 段定稿链(PCal/归档)必然继续"—— PCal 必发是客户端 REPLACED 语义的契约前提。
pub(crate) const PARAGRAPH_RERUN_TIMEOUT: Duration = Duration::from_secs(15);

/// 句任务的产出:batch 识别结果(回填段落句集,供 live/定稿整流)。
pub(crate) struct SentenceOutcome {
    pub(crate) sentence_id: SentenceId,
    pub(crate) batch_text: Option<String>,
    #[allow(dead_code)]
    pub(crate) asr_ms: u64,
}

/// 句任务:取句 clip(AudioStore,EOS → 段关闭之间存活)→ 阻塞 recognize → 回传
/// `BatchSentence`(经 turn 通道回主循环 emit,单点)。失败/空文本 = 合法 None
/// (消费端回退流式)。
pub(crate) async fn sentence_task(
    s1: Arc<OnnxStage1Recognizer>,
    paragraph_id: ParagraphId,
    sentence_id: SentenceId,
    audio_id: crate::AudioId,
    sr: u32,
    turn: TurnTx,
) -> SentenceOutcome {
    let out = tokio::task::spawn_blocking(move || {
        let pcm = s1.audio_store().concat(&[audio_id]);
        let start = chrono::Local::now();
        let t0 = Instant::now();
        let text = s1.recognize_once(&pcm, sr, "句级", paragraph_id);
        (text, t0.elapsed().as_millis() as u64, start, chrono::Local::now())
    })
    .await
    .unwrap_or_else(|_| (None, 0, chrono::Local::now(), chrono::Local::now()));
    let (text, asr_ms, start, end) = out;
    info!(paragraph_id, sentence_id,
        start = %wall(start), end = %wall(end), asr_ms,
        batch = text.as_deref().unwrap_or("(none)"),
        "batch[sentence] 完成");
    if let Some(text) = &text {
        let _ = turn.send(TurnEvent::BatchSentence {
            paragraph_id,
            sentence_id,
            text: text.clone(),
        });
    }
    SentenceOutcome { sentence_id, batch_text: text, asr_ms }
}

/// live 联合整流任务(每句 batch 完成后一次 —— BS 到达时由主循环触发;架构需求
/// "batch 完成 → 之后纠偏,先后明确")。`prev` = 本段
/// 上一次 live 任务句柄 —— 链式 await 保证段内 SC 按 Batch 顺序(段落生长序)串行,
/// 后到的 SC 覆盖更早的(REPLACED)。阻塞 LLM 在 `spawn_blocking` + 校准器 Mutex 串行。
pub(crate) async fn live_calibration_task(
    prev: Option<tokio::task::JoinHandle<()>>,
    paragraph_id: ParagraphId,
    // 触发本次纠偏的句 id(BS 到达的那句)—— 随事件带给前端:SC 是段落级快照,
    // 覆盖上界就是它;前端零派生状态即可知道"该覆盖谁"(round20b)。
    up_to: SentenceId,
    sentences: Vec<VadSentence>,
    calibrate: Arc<Mutex<Box<dyn Stage2Calibrator>>>,
    turn: TurnTx,
) {
    if let Some(h) = prev {
        let _ = h.await;
    }
    let t = Instant::now();
    let start = chrono::Local::now();
    let calibrated = tokio::task::spawn_blocking(move || {
        calibrate.lock().unwrap().calibrate_paragraph(paragraph_id, &sentences)
    })
    .await
    .unwrap_or_default();
    let end = chrono::Local::now();
    let route_ms = t.elapsed().as_secs_f64() * 1000.0;
    info!(paragraph_id, sentence_id = up_to,
        start = %wall(start), end = %wall(end), route_ms = route_ms.round() as u64,
        calibrated = %calibrated, "纠偏[sentence]");
    let _ = turn.send(TurnEvent::SentenceCalibration {
        paragraph_id,
        sentence_id: up_to,
        calibrated,
        route_ms,
    });
}

/// 段任务的等待集(就绪门):句任务 handles(回填句 batch)+ live 整流链尾
/// (该段全部 SC 先于 PCal 的契约保证)。
pub(crate) struct ParagraphWaits {
    pub(crate) sentences: Vec<tokio::task::JoinHandle<SentenceOutcome>>,
    pub(crate) live: Option<tokio::task::JoinHandle<()>>,
}

/// 段任务:join 等待集(就绪门,回填句 batch;SC 先于 PCal)→ 段级重跑(多句段落)→
/// 定稿整流一次 → 归档 → `ParagraphCalibration`。全部阻塞调用(LLM/ASR/文件 IO)
/// 都在 `spawn_blocking` 里,任务并发不占事件循环。
pub(crate) async fn paragraph_task(
    paragraph: VadParagraph,
    sr: u32,
    waits: ParagraphWaits,
    calibrate: Arc<Mutex<Box<dyn Stage2Calibrator>>>,
    run_paragraph_batch: RunParagraphBatch,
    storage: Option<Arc<Storage>>,
    turn: TurnTx,
) {
    let paragraph_id = paragraph.id;
    let mut acc: Vec<VadSentence> = paragraph.sentences.clone();

    // 就绪门 = join!:回填各句 batch(定稿整流的输入)。live 整流已在 Batch 臂持续
    // 完成(live_calibration_task),这里等链尾收束 —— 该段全部 SC 必先于 PCal。
    for h in waits.sentences {
        let Ok(out) = h.await else { continue };
        if let Some(s) = acc.iter_mut().find(|s| s.id == out.sentence_id) {
            s.batch_text = out.batch_text;
        }
    }
    if let Some(live) = waits.live {
        let _ = live.await;
    }

    // ★ 回填写回段落实体(round16b 修复):ParagraphEdge 快照里各句 `batch_text` 恒
    // None(tracker 的副本从不更新)。不写回则定稿/归档的 `best_text` 静默回退**流式**
    // —— PassThrough 下 PCal 直接发出流式拼接,经 REPLACED 把 finals 里已到手的
    // batch 占位换回流式文本(实测"batch 后退回流式"的服务端根因;单句段落无 BP
    // 掩盖,完全裸露)。round12 之前的 Finalizer 有此 patch,tokio 重写时丢失。
    let mut paragraph = paragraph;
    paragraph.sentences = acc;

    // 段级重跑(权威 raw):多句段落才跑(单句的拼接 PCM 与句级完全相同,复用句级 batch)。
    // ★ 必须 spawn_blocking(round17 实测 panic 根因):HttpAsr 是 reqwest::blocking
    // (内部持 runtime),裸调在 async 上下文触发 "Cannot drop a runtime …" panic →
    // 段任务崩死,PCal/归档永久丢失(前端:某段永远等不到定稿)。超时兜底:重跑
    // 挂死/panic 也保证定稿链继续(None 回退句级 batch)—— "PCal 必发"是契约。
    if paragraph.sentences.len() > 1 {
        let pcm = Arc::clone(&paragraph.pcm);
        let run = Arc::clone(&run_paragraph_batch);
        // 计时进闭包(round25):起止墙钟 + 耗时;事件字段 batch_asr_ms 落真值。
        let (batch_text, rerun_ms) = match tokio::time::timeout(
            PARAGRAPH_RERUN_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let start = chrono::Local::now();
                let t0 = Instant::now();
                let text = run(&pcm, sr, paragraph_id);
                let ms = t0.elapsed().as_millis() as u64;
                info!(paragraph_id, start = %wall(start), end = %wall(chrono::Local::now()),
                    rerun_ms = ms, batch = text.as_deref().unwrap_or("(none)"),
                    "batch[paragraph] 整段重跑完成");
                (text, ms)
            }),
        )
        .await
        {
            Ok(Ok((t, ms))) => (t, ms),
            Ok(Err(e)) => {
                warn!(error = %e, paragraph_id, "段级重跑任务 panic → 回退句级 batch");
                (None, 0)
            }
            Err(_) => {
                warn!(paragraph_id, "段级重跑超时 → 回退句级 batch(阻塞线程残留,无碍)");
                (None, 0)
            }
        };
        paragraph.batch_asr_ms = rerun_ms;
        if let Some(text) = batch_text {
            paragraph.batch_text = Some(text.clone());
            let _ = turn.send(TurnEvent::BatchParagraph { paragraph_id, text });
        }
    }

    // 定稿整流:一次 LLM(全句 best_text —— 句级 batch 已全部回填,live 整流给不了的)。
    let t = Instant::now();
    let start = chrono::Local::now();
    let calibrated = {
        let cal = Arc::clone(&calibrate);
        let para = paragraph.clone();
        tokio::task::spawn_blocking(move || cal.lock().unwrap().finalize_paragraph(&para))
            .await
            .unwrap_or_default()
    };
    let end = chrono::Local::now();
    let route_ms = t.elapsed().as_secs_f64() * 1000.0;
    info!(
        paragraph_id = paragraph.id,
        start = %wall(start), end = %wall(end),
        at_s = (paragraph.start_s * 10.0).round() / 10.0,
        sentences = paragraph.sentences.len(),
        batch = %paragraph.batch_text.clone().unwrap_or_default(),
        streaming = %paragraph.streaming_text,
        calibrated = %calibrated,
        "纠偏[paragraph]"
    );
    // 归档:段落 PCM → audio archive,三份文本 → day log + ring。阻塞文件 IO
    // 同样进 spawn_blocking(与重跑同类:不得占/崩 async 执行器)。
    if let Some(storage) = &storage {
        let record = FinalTurn {
            paragraph_id: paragraph.id,
            at_s: paragraph.start_s,
            duration_ms: paragraph.duration_ms(),
            raw_text: paragraph.best_text().into_owned(),
            streaming_text: paragraph.streaming_text.clone(),
            calibrated: calibrated.clone(),
            route_ms,
            pcm: (*paragraph.pcm).clone(),
        };
        let st = Arc::clone(storage);
        let _ = tokio::task::spawn_blocking(move || st.record_final(record)).await;
    }
    let _ = turn.send(TurnEvent::ParagraphCalibration { paragraph_id, calibrated, route_ms });
}
