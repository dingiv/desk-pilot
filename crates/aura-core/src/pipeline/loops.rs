//! loops — **两个循环**(round27 并屋):
//! ① `main_loop`:select! 主循环(桥①拉流 spawn_blocking、桥②大脑 spawn、
//!    唯一 on_turn 发射点 —— SF/BS/SC/PC/PCal 全序在此排队);
//! ② `consume_loop`:大脑(消费循环)—— 吃 FrontEvent 队列,tracker 分句/段落
//!    决策、Finalize 握手、flush、STALE 看门狗。
//! 主循环消费大脑产出(Stage1Event)与任务回传(TurnEvent),两循环 = 编排本体。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc as t_mpsc, Notify};

use tracing::{debug, info, warn};

use crate::audio_store::AudioStore;
use crate::hub::Storage;
use crate::pipeline::batch::{
    emit_turn, live_calibration_task, paragraph_task, sentence_task,
};
use crate::pipeline::calibrator::Stage2Calibrator;
use crate::pipeline::front::speech_pending;
use crate::pipeline::resources::OnnxStage1Recognizer;
use crate::pipeline::stream::{
    await_finalize, drain_stream_out, run_stream_worker, STALE_SESSION_RESET,
};
use crate::pipeline::tracker::ParagraphTracker;
use crate::pipeline::types::{
    FrontEvent, ParagraphWaits, PartialMirror, RunParagraphBatch, SentenceOutcome,
    SettledParagraph, StreamCmd, StreamFinal, TurnEvent,
};
use crate::{AudioId, ParagraphId, SentenceId, Stage1Event, VadEventKind, VadParagraph, VadSentence};

// ── select! 两臂的处理器(round24 R4:臂体拆出,循环只剩分派)─────────────────
// 状态账本(三张表)与处理器分离:循环 = 事件分派,处理器 = 各事件的编排语义。

/// select! 处理器的共享依赖(run 存续期不变;round24:收进一个结构,处理器签名不再长参)。
struct Ctx<F> {
    s1: Arc<OnnxStage1Recognizer>,
    s2: Arc<Mutex<Box<dyn Stage2Calibrator>>>,
    storage: Option<Arc<Storage>>,
    turn: crate::pipeline::types::TurnTx,
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

pub(crate) async fn main_loop<F>(
    s1: OnnxStage1Recognizer,
    s2: Arc<Mutex<Box<dyn Stage2Calibrator>>>,
    storage: Option<Arc<Storage>>,
    running: Arc<AtomicBool>,
    resume: Arc<Notify>,
    on_turn: F,
) where
    F: Fn(TurnEvent) + Send + Sync + 'static,
{
    let on_turn = Arc::new(on_turn);
    // s1 被消费线程、任务(recognize_once)共享 —— Arc。
    let stage1 = Arc::new(s1);

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
                consume_loop(&s1, &mut move |ev| {
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


async fn pop_front_event(
    front_q: &Mutex<VecDeque<FrontEvent>>,
    notify: &Notify,
    timeout: Option<Duration>,
) -> Option<FrontEvent> {
    {
        let mut g = front_q.lock().unwrap();
        if let Some(fe) = g.pop_front() {
            return Some(fe);
        }
    }
    // 先注册 waiter 再复查一次队列(双保险;permit 语义本身已防丢唤醒)。
    let notified = notify.notified();
    {
        let mut g = front_q.lock().unwrap();
        if let Some(fe) = g.pop_front() {
            return Some(fe);
        }
    }
    match timeout {
        Some(t) => {
            let _ = tokio::time::timeout(t, notified).await;
        }
        None => notified.await,
    }
    // 醒来(通知或截止)→ 终检一次队列(截止竞态窗口内可能刚 push)。
    front_q.lock().unwrap().pop_front()
}

/// Turn settled spans into a [`VadParagraph`] and emit `ParagraphEdge`: concat the clips from
/// the store (once — the paragraph keeps the shared `Arc`), then evict the clips. The
/// event carries `batch_text: None` (in-flight); the paragraph re-run is built by the
/// pipeline's paragraph task (round12 起任务结构自管 batch). An all-discarded paragraph
/// (no sentences) emits nothing and just vanishes.
fn emit_paragraph_edge(
    settled: SettledParagraph,
    store: &AudioStore,
    sr: u32,
    on_event: &mut dyn FnMut(Stage1Event),
) {
    if settled.sentences.is_empty() {
        return;
    }
    let ids: Vec<AudioId> = settled.sentences.iter().map(|s| s.audio_id).collect();
    let pcm = Arc::new(store.concat(&ids));
    let streaming_text = settled
        .sentences
        .iter()
        .map(|s| s.streaming_text.as_str())
        .collect::<String>();
    let start_s = settled.sentences.first().map(|s| s.start_s).unwrap_or(0.0);
    let end_s = settled.sentences.last().map(|s| s.end_s).unwrap_or(0.0);
    // ★ 顺序不变式(竞态防护):`ParagraphEdge` 先落 stage2 FIFO 通道(占位建 pending),
    // 段重跑由 pipeline 在事件之后自建任务 —— 结果必在事件之后。
    on_event(Stage1Event::ParagraphEdge {
        paragraph: VadParagraph {
            id: settled.paragraph_id,
            sentences: settled.sentences,
            start_s,
            end_s,
            streaming_text,
            // ASYNC re-run: None on this event; the pipeline's paragraph task patches it.
            batch_text: None,
            batch_asr_ms: 0,
            pcm: Arc::clone(&pcm),
        },
        sr,
    });
    // 段重跑/单句免重跑的调度决策都在 pipeline 段任务侧(单句段落复用句级 batch)。
    // The paragraph's Arc PCM is now the only remaining copy — release the per-sentence clips
    // (the re-run job shares the paragraph's Arc, so eviction is safe).
    store.evict(&ids);
}

/// 取帧结果:拿到一帧去处理,或 park 后重跑循环(截止/节流触发)。
enum FrameResult {
    Frame(FrontEvent),
    Parked,
}

/// 取一条 FrontEvent 处理,或 park 后重跑循环。队列有货直接取;空则 park 到
/// 下一截止(断流喂静音已随 VAD 住拉流线程,R4)。
async fn drain_frame(
    front_q: &Mutex<VecDeque<FrontEvent>>,
    notify: &Notify,
    wake_at: Option<Duration>,
) -> FrameResult {
    // 作用域块取帧:guard 绝不跨 await(generator Send 分析对显式 drop 保守,
    // 作用域块是可靠写法)。
    if let Some(fe) = front_q.lock().unwrap().pop_front() {
        return FrameResult::Frame(fe);
    }
    // Park until the ingest pushes or the next deadline — 无轮询,空闲零唤醒.
    match pop_front_event(front_q, notify, wake_at).await {
        Some(fe) => FrameResult::Frame(fe),
        None => FrameResult::Parked,
    }
}

/// 定稿一个 VAD 句(EOS 臂):流式任务回执(`StreamFinal`,调用前经 `await_finalize`
/// → streaming_text,句 PCM 入 store(共享 `Arc`),**入队句级 batch job(异步——消费
/// `Arc`),**入队句级 batch job(异步——消费循环不阻塞)**,emit `Batch`(`batch_text: None`)
/// 及可能的 `ParagraphEdge`。`fallback_pcm` = 流式未配置时的 VAD edge-extended 句。
///
/// 噪声句不再在 EOS 丢弃:batch 异步后 EOS 时刻只有流式文本,若流式空就丢弃,会丢掉
/// "流式没听出、batch 能听出"的真实语音(吞句的另一形态)。空句无文本贡献,由段落折叠
/// 自然吸收;停滞幻觉由 8s 看门狗在下一句前清掉。
fn finalize_sentence(
    audio_store: &AudioStore,
    stream: StreamFinal,
    onset_s: f64,
    tracker: &mut ParagraphTracker,
    cur_sentence: &mut SentenceId,
    sr: u32,
    end_s: f64,
    fallback_pcm: Vec<i16>,
    on_event: &mut dyn FnMut(Stage1Event),
) {
    // 句 PCM = 流式任务累积的完整音频(含句首 soft onset)——与流式听到的完全一致,
    // 区别只在 batch 一次整句听(大块)vs 流式逐帧听(小块)。`pcm: None` = 流式未
    // 配置 → fallback VAD edge-extended 句。`Arc`:store / batch job 共享同一份分配,零拷贝。
    let sentence_pcm: Arc<Vec<i16>> = Arc::new(stream.pcm.unwrap_or(fallback_pcm));
    let streaming_text = stream.text;
    // start_s = 起音墙钟(rising edge,与 on_speech_onset/check_settle 同一把
    // 量尺);退化兜底(理论上不可达:每句 EOS 前必有翻转)才用 end−PCM 反推。
    let start_s = if onset_s > 0.0 {
        onset_s
    } else {
        (end_s - sentence_pcm.len() as f64 / sr as f64).max(0.0)
    };
    let sentence_id = *cur_sentence;
    let sentence = VadSentence {
        id: sentence_id,
        audio_id: audio_store.insert(Arc::clone(&sentence_pcm)),
        start_s,
        end_s,
        streaming_text: streaming_text.clone(),
        // ASYNC batch: the pass runs on the batch worker thread; the result arrives via
        // SentenceBatchReady. None here is the in-flight state (== the old "batch failed"
        // state for consumers — best_text falls back to streaming either way).
        batch_text: None,
    };
    let (settled, paragraph_id, sentences) = tracker.on_eos(sentence);
    // A big gap settled the previous paragraph FIRST — emit it before this sentence's Batch.
    if let Some(s) = settled {
        emit_paragraph_edge(s, audio_store, sr, on_event);
    }
    // 句级日志(debug):段落/段 id、音频时长、两路文本(异步 batch 尚未返回)、会话喂帧数。
    if let Some(s) = sentences.last() {
        debug!(
            paragraph_id = paragraph_id,
            sentence_id = s.id,
            time_ms = ((s.end_s - s.start_s) * 1000.0).round() as u64,
            fed = stream.fed,
            streaming = %s.streaming_text,
            "句结束"
        );
    }
    // Final stream fragment: the sentence's DEFINITIVE streaming text (live partials only
    // decode up to the last throttle frame; finalize is authoritative).
    if let Some(s) = sentences.last().filter(|s| !s.streaming_text.is_empty()) {
        on_event(Stage1Event::StreamFragment {
            paragraph_id,
            sentence_id: s.id,
            text: s.streaming_text.clone(),
            at_s: end_s,
        });
    }
    // ★ 顺序不变式(竞态防护):先把 `Batch` 事件发上 stage2 通道,再入队句级 batch job。
    // 二者是不同 channel —— 若先入队 job,worker 可能在 `Batch` 被 Finalizer 处理前就产出
    // `SentenceBatchReady`,Finalizer 找不到该段条目而丢弃它,`ready` 永不达 `expected` →
    // 该段悬挂(永不就绪)。先发 `Batch`(占位建 pending)后入队 job,则结果必在 `Batch`
    // 之后落到同一条 stage2 FIFO 通道 → 就绪计数必到齐。
    on_event(Stage1Event::Batch {
        paragraph_id,
        sentences,
        sr,
    });
    // 句级 batch 由 pipeline 在 `Batch` 事件处理时自建任务(round12 任务结构自管,
    // 经 recognize_once;audio_store/事件已带共享 PCM)。
}

/// 下一次唤醒截止:最早的真实定时器,或 None(无定时 → 无限期挂起等音频)。
/// `flush_pending`:主动归档挂起中 → 最长 50ms 后醒来重试(EOS 一到立即归档,
/// 否则 condvar park 到 settle deadline 才醒,flush 延迟退化回 merge_gap)。
fn next_wake_at(
    tracker: &ParagraphTracker,
    mirror: PartialMirror,
    now_s: f64,
    speaking: bool,
    flush_pending: bool,
) -> Option<Duration> {
    let mut wake_at: Option<Duration> = None;
    if flush_pending {
        wake_at = Some(Duration::from_millis(50));
    }
    if let Some(d) = tracker.settle_deadline(now_s, speaking) {
        let d = Duration::from_secs_f64(d.max(0.05));
        wake_at = Some(wake_at.map_or(d, |w| w.min(d)));
    }
    if mirror.nonempty {
        let d = STALE_SESSION_RESET.saturating_sub(mirror.last_change.elapsed());
        wake_at = Some(wake_at.map_or(d, |w| w.min(d)));
    }
    wake_at
}

// R5 已整改(2026-08-30 batch 异步化): 轮询已除(ring 挂 Notify,仅真实截止时间唤醒,
// 空闲零唤醒);batch 调用移出消费线程 —— EOS/settle 只发事件(微秒级),阻塞的
// recognize 由 Pipeline 的句任务执行。消费循环不再被 batch 阻塞:流式/VAD/check_settle
// 持续运行,修复了"间隔 1–3.5s 首句被吞"(batch 阻塞期间墙钟越过 merge_gap 导致段落误切)。
// round14b:消费循环本体 async —— 帧等待 = Notify(park 空闲零唤醒),VAD(每 32ms,
// 微秒级)与流式解码(0.3s 节流)内联在 executor 上,量级是协作式调度的标准负载。
// round21:流式解码再拎出 —— accept/decode 全部移入独立 tokio::task(async fn,
// executor 协作调度),消费循环只转发帧/指令、发射事件;VAD/分句/段落定稿从此与
// 流式推理零共享。
// round21b:RPITIT 结案 —— 固有 `async fn run`(原 trait 已删)。
/// 跑消费循环直到 `running` 被置 false(idle 深度睡眠)→ 返回。daemon 恢复时重新调用。
pub(crate) async fn consume_loop(
    s1: &OnnxStage1Recognizer,
    on_event: &mut (dyn FnMut(Stage1Event) + Send),
) {
        let sr = 16000u32;
        // D9 共同时钟:与拉流线程的 VAD 检测同一原点(s1.start 构造时定,
        // run() 重入不换尺)。
        let start = s1.start;
        let mut last_diag = Instant::now();
        let mut frames_in = 0u64;

        // round21:流式模型拎出消费循环 —— 独立 tokio::task(async fn,executor 协作
        // 调度,不占阻塞线程)。本循环只转发帧/起音/重置/定稿指令;partial 回传后仍
        // 从这里发射,事件全序(partial 先于 Batch/ParagraphEdge)不变。VAD(每帧,
        // 先跑)从此不与流式解码抢同一个任务。`cur_sentence` 由回溯式 SOS 分配
        // (与 EOS 同批到达)。
        let (stream_tx, cmd_rx) = t_mpsc::unbounded_channel();
        let (out_tx, mut stream_rx) = t_mpsc::unbounded_channel();
        tokio::spawn(run_stream_worker(Arc::clone(&s1.mgr), sr, cmd_rx, out_tx));
        let has_stream = s1.mgr.streaming_asr().is_some();
        // R3:流式端口交给拉流线程(门控/Onset/Feed 直发);本循环的 tx 只发
        // Finalize/Reset。退出(⓪)时摘除,worker 随通道关闭而终。
        if has_stream {
            *s1.stream_port.lock().unwrap() = Some(stream_tx.clone());
        }
        let mut mirror = PartialMirror::empty();
         let mut tracker = ParagraphTracker::new(s1.merge_gap_s);
         let mut cur_sentence: SentenceId = 0;
         // 本句起音墙钟(FrontEvent.onset 随行,R3 起由拉流线程记录)—— round26
         // 量尺:settle 判定与 sentence.start_s 都用它,不用 end−PCM 反推
         // (PCM 含 0.5s lead-in、不含尾随 1s 静音,反推偏晚 ~0.5s → 间隔虚增
         // → 与起音判定矛盾的"同句中途换段" bug)。
         let mut onset_at: f64 = 0.0;
         // VAD 检测器(R1:Stage0VAD trait,SileroVAD;结构体字段共享):
         // 喂帧 + detected 快照 + 起音盲区门状态都在前端侧,本循环只消费它的输出。
         // speaking 抑制 glue = speech_pending 自由函数(partial 是 ASR 概念,不进 Stage0)。

        loop {
            // ⓪ idle 深度睡眠:running=false → 退出消费循环。daemon 断开 scout,下一个客户端
            //   连接时置回 true 并重新调用 run() 恢复识别。摘除流式端口:拉流线程
            //   见 None 即停转发(帧照喂 VAD/入队),流式 worker 随通道关闭而终。
            if !s1.running.load(Ordering::Relaxed) {
                s1.stream_port.lock().unwrap().take();
                return;
            }
            // ① 连接开关:scout 暂停时挂起等音频,不做 VAD/ASR
            if !s1.active.load(Ordering::Relaxed) {
                let _ = pop_front_event(&s1.front_q, &s1.front_notify, None).await;
                continue;
            }

            // ② 时间驱动检查:主动归档 / 段落定稿 / 停滞看门狗 / 诊断
            let now_s = start.elapsed().as_secs_f64();
            // ②′ 冲刷流式任务回传:partial → 事件;镜像刷新(speaking/看门狗/断流判据)
            drain_stream_out(&mut stream_rx, &tracker, now_s, &mut mirror, on_event);
            // `speaking` 抑制段落按墙钟定稿——回溯式 VAD 的下一句 SOS 尚未到达,若
            // 定稿会把下一句错划进新段落。组合判定:partial 非空 **或** 起音盲区边际
            // 内(detected() 近期见过;见 VOICE_SETTLE_MARGIN)。
            let speaking = speech_pending(mirror.nonempty, s1.vad.last_voice_at(), now_s);
            // R4:partial 快照给拉流线程(断流喂静音判据);一迭代粒度足够
            // (判据窗口 2s,滞后 ≤1 帧 32ms)。
            s1.partial_live.store(mirror.nonempty, Ordering::Release);
            // 用户侧主动归档(IME 分字符 = "我说完了"):跳过 merge_gap 剩余等待立即整段
            // batch。说话中(EOS 未到)保持挂起下一 tick 重试 —— 立即切段会截断尾音;
            // 无段落则消费掉标记(空按,不让陈旧 flush 影响之后的语音)。
            if s1.flush_paragraph.load(Ordering::Acquire) && !speaking {
                match tracker.force_settle() {
                    Some(settled) => {
                        s1.flush_paragraph.store(false, Ordering::Release);
                        info!(
                            paragraph_id = settled.paragraph_id,
                            sentences = settled.sentences.len(),
                            "flush: 主动归档(跳过 merge_gap 等待)"
                        );
                        emit_paragraph_edge(settled, &s1.audio_store, sr, on_event);
                        let _ = stream_tx.send(StreamCmd::Reset); // 段落边界重置会话
                        mirror.nonempty = false;
                    }
                    None if !tracker.has_open_paragraph() => {
                        s1.flush_paragraph.store(false, Ordering::Release);
                    }
                    None => {} // 句进行中 → 挂起,等 EOS 后下一 tick 强制定稿
                }
            }
            if let Some(settled) = tracker.check_settle(now_s, speaking) {
                emit_paragraph_edge(settled, &s1.audio_store, sr, on_event);
                let _ = stream_tx.send(StreamCmd::Reset); // 段落边界重置会话
                mirror.nonempty = false;
            }
            if mirror.nonempty && mirror.last_change.elapsed() >= STALE_SESSION_RESET {
                warn!(
                    stale_s = mirror.last_change.elapsed().as_secs(),
                    "流式会话停滞重置——VAD 未定句的微弱音频不残留到下一句"
                );
                let _ = stream_tx.send(StreamCmd::Reset);
                mirror.nonempty = false;
            }
            if last_diag.elapsed() >= Duration::from_secs(3) {
                let has_partial = mirror.nonempty;
                debug!(
                    frames = frames_in,
                    front = s1.front_q.lock().unwrap().len(),
                    has_partial,
                    "stage1 diag"
                );
                last_diag = Instant::now();
            }

            // ③ 取 FrontEvent:队列有货直接取;空则 park 等音频/截止
            //    (断流喂静音已住拉流线程,R4)。
            let wake_at = next_wake_at(
                &tracker,
                mirror,
                now_s,
                speaking,
                s1.flush_paragraph.load(Ordering::Acquire),
            );
            let front = match drain_frame(&s1.front_q, &s1.front_notify, wake_at).await {
                FrameResult::Frame(fe) => fe,
                FrameResult::Parked => continue,
            };
            frames_in += 1;

            // ④⑤ 检测+门控已下沉拉流线程(R3):detected/事件/onset 随 FrontEvent
            //    同批到达(静音兜底帧由 drain_frame 内联喂)。大脑只做决策:
            //    rising-edge 开段(真键前置)+ 分句/段落。
            let FrontEvent { detected: _v, events, onset } = front;
            if let Some(at) = onset {
                // ★ 起音即开段(§7-B 根治):rising edge 立刻分配真实段落 id ——
                // 此后本段所有 partial/事件都携带真键,幽灵段(预测键)不复存在。
                tracker.on_speech_onset(at);
                onset_at = at; // 本句起音墙钟(EOS 定稿的 settle 量尺)
            }

            // ⑥ 分句:SOS 分配句号(段落已在起音开启,SOS 只补 sentence id);
            //    EOS 定稿成句(batch + ParagraphEdge)
            for ev in events {
                match ev.kind {
                    VadEventKind::StartOfSpeech => {
                        cur_sentence = tracker.on_sos(start.elapsed().as_secs_f64())
                    }
                    VadEventKind::EndOfSpeech => {
                        let end_s = start.elapsed().as_secs_f64();
                        // 定稿交接(唯一同步点:每句一次,B 侧本地 finalize):
                        // 发 Finalize → 挂起等回执(同通道,partial 依序先发)。
                        // B 收到 Finalize 后不再有 Feed 在途 → Finalized 必为最后一条。
                        let _ = stream_tx.send(StreamCmd::Finalize);
                        mirror.nonempty = false; // 会话已被 B 取走重置
                        let stream =
                            await_finalize(&mut stream_rx, &tracker, end_s, &mut mirror, on_event).await;
                        let onset = std::mem::take(&mut onset_at);
                        finalize_sentence(
                &s1.audio_store,

                            stream,
                            onset,
                            &mut tracker,
                            &mut cur_sentence,
                            sr,
                            end_s,
                            ev.pcm.clone(),
                            on_event,
                        );
                     }
                 }
             }
         }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::calibrator::{LlmInput, PassThroughCalibrator, Stage2CalibratorImpl};
    use crate::pipeline::spec::{AsrSpec, LlmSpec, PipelineSpec, StreamSpec, VadSpec};
    use crate::pipeline::resources::Stage1Config;
    use crate::pipeline::{stage1_config, stage2_calibrator};
    use crate::pipeline::batch::describe_turn;
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
