//! consume — Stage1 消费循环(round23 从 recognizer.rs 拆出):
//! `run` = 瘦调度(ring 取帧 → VAD/时间检查分派);帧等待(`wait_frame`)、句定稿
//! (`finalize_sentence`)、段落边发射(`emit_paragraph_edge`)、唤醒截止(`next_wake_at`)
//! 都在本文件。资源/配置/生命周期在 recognizer.rs;编排在 mod.rs。

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc as t_mpsc, Notify};

use tracing::{debug, info, warn};

use crate::audio_store::AudioStore;
use crate::buffer::AudioRing;
use crate::pipeline::vad::VadFront;
use crate::pipeline::stream::{
    await_finalize, drain_stream_out, run_stream_worker, PartialMirror, StreamCmd, StreamFinal,
    STALE_SESSION_RESET,
};
use crate::pipeline::tracker::{ParagraphTracker, SettledParagraph};
use crate::{AudioId, SentenceId, Stage1Event, VadEventKind, VadParagraph, VadSentence};
use dp_models::onnx::WINDOW;

/// VAD 门控流式的 lead-in 帧数(每帧 32ms):detected() 翻转起音时补喂最近 ~0.5s 的帧,
/// 让 soft onset 进入流式/batch(Silero 要几帧过阈值,detected 翻转晚于真实起音)。
const LEAD_IN_FRAMES: usize = 16;

use crate::pipeline::recognizer::OnnxStage1Recognizer;

/// Wait until a full Silero frame is available in the ring (wakes on the ingest's
/// `Notify`). `timeout: Some` additionally caps the wait — `None` return means the deadline
/// fired (the caller re-runs its time-based checks); `timeout: None` parks until audio
/// arrives (no timer at all — nothing time-based is pending).
///
/// **async(round14b)**:消费循环原生异步 —— 唤醒源从 Condvar 换成 `tokio::sync::Notify`
/// (permit 语义:检查 ring 之后、`await` 之前的 push 不会丢唤醒 —— notify_one 存的
/// permit 会让 `notified()` 立即就绪)。std Mutex 保留:临界区是纳秒级的 ring 操作,
/// async 里短暂持锁是标准做法。
async fn wait_frame(
    ring: &Mutex<AudioRing>,
    notify: &Notify,
    frame_samples: usize,
    timeout: Option<Duration>,
) -> Option<Vec<i16>> {
    {
        let mut g = ring.lock().unwrap();
        if g.has_frame(frame_samples) {
            return Some(g.drain(frame_samples));
        }
    }
    // 先注册 waiter 再复查一次 ring(双保险;permit 语义本身已防丢唤醒)。
    let notified = notify.notified();
    {
        let mut g = ring.lock().unwrap();
        if g.has_frame(frame_samples) {
            return Some(g.drain(frame_samples));
        }
    }
    match timeout {
        Some(t) => {
            let _ = tokio::time::timeout(t, notified).await;
        }
        None => notified.await,
    }
    // 醒来(通知或截止)→ 终检一次 ring(截止竞态窗口内可能刚 push)。
    let mut g = ring.lock().unwrap();
    if g.has_frame(frame_samples) {
        Some(g.drain(frame_samples))
    } else {
        None
    }
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
    Frame(Vec<i16>),
    Parked,
}

impl OnnxStage1Recognizer {
    /// 取一帧(32ms)处理,或 park 后重跑循环。ring 有帧直接取;空则等音频/截止,
    /// 断流>2s 且有 partial 时喂合成静音逼 VAD EOS(100ms 节流,避免 CPU 空转)。
    async fn drain_frame(
        &self,
        ring_empty_since: &mut Option<Instant>,
        partial_nonempty: bool,
        last_silence_feed: &mut Instant,
        wake_at: Option<Duration>,
    ) -> FrameResult {
        // 作用域块取帧:guard 绝不跨 await(generator Send 分析对显式 drop 保守,
        // 作用域块是可靠写法)。
        let ready = {
            let mut g = self.ring.lock().unwrap();
            g.has_frame(WINDOW).then(|| g.drain(WINDOW))
        };
        if let Some(f) = ready {
            *ring_empty_since = None;
            return FrameResult::Frame(f);
        }
        ring_empty_since.get_or_insert_with(Instant::now);
        let since = *ring_empty_since.as_ref().unwrap();
        let has_partial = partial_nonempty;
        if since.elapsed() > Duration::from_secs(2) && has_partial {
            // 断流:喂合成静音让 VAD 发 EOS(每 100ms 至多一帧,~1s 静音约 3s 墙钟)
            if last_silence_feed.elapsed() >= Duration::from_millis(100) {
                *last_silence_feed = Instant::now();
                debug!("ring empty > 2s during speech — feeding silence to force VAD EOS");
                FrameResult::Frame(vec![0i16; WINDOW])
            } else {
                match wait_frame(
                    &self.ring,
                    &self.ring_notify,
                    WINDOW,
                    Some(Duration::from_millis(100)),
                )
                .await
                {
                    Some(f) => {
                        *ring_empty_since = None;
                        FrameResult::Frame(f)
                    }
                    None => FrameResult::Parked,
                }
            }
        } else {
            // Park until the ingest pushes or the next deadline — 无轮询,空闲零唤醒.
            match wait_frame(&self.ring, &self.ring_notify, WINDOW, wake_at).await {
                Some(f) => {
                    *ring_empty_since = None;
                    FrameResult::Frame(f)
                }
                None => FrameResult::Parked,
            }
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
        &self,
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
            audio_id: self.audio_store.insert(Arc::clone(&sentence_pcm)),
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
            emit_paragraph_edge(s, &self.audio_store, sr, on_event);
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
}

/// 下一次唤醒截止:最早的真实定时器,或 None(无定时 → 无限期挂起等音频)。
/// `flush_pending`:主动归档挂起中 → 最长 50ms 后醒来重试(EOS 一到立即归档,
/// 否则 condvar park 到 settle deadline 才醒,flush 延迟退化回 merge_gap)。
fn next_wake_at(
    tracker: &ParagraphTracker,
    mirror: PartialMirror,
    ring_empty_since: Option<Instant>,
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
    if let Some(since) = ring_empty_since {
        if mirror.nonempty {
            // Silence-feed deadline: force VAD EOS if the source dropped mid-utterance.
            let d = Duration::from_secs(2).saturating_sub(since.elapsed());
            wake_at = Some(wake_at.map_or(d, |w| w.min(d)));
        }
    }
    wake_at
}

impl OnnxStage1Recognizer {
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
    pub async fn run(&self, on_event: &mut (dyn FnMut(Stage1Event) + Send)) {
            let sr = 16000u32;
            let start = Instant::now();
            let mut last_diag = Instant::now();
            let mut frames_in = 0u64;

            // round21:流式模型拎出消费循环 —— 独立 tokio::task(async fn,executor 协作
            // 调度,不占阻塞线程)。本循环只转发帧/起音/重置/定稿指令;partial 回传后仍
            // 从这里发射,事件全序(partial 先于 Batch/ParagraphEdge)不变。VAD(每帧,
            // 先跑)从此不与流式解码抢同一个任务。`cur_sentence` 由回溯式 SOS 分配
            // (与 EOS 同批到达)。
            let (stream_tx, cmd_rx) = t_mpsc::unbounded_channel();
            let (out_tx, mut stream_rx) = t_mpsc::unbounded_channel();
            tokio::spawn(run_stream_worker(Arc::clone(&self.mgr), sr, cmd_rx, out_tx));
            let has_stream = self.mgr.streaming_asr().is_some();
            let mut mirror = PartialMirror::empty();
            let mut ring_empty_since: Option<Instant> = None;
             let mut tracker = ParagraphTracker::new(self.merge_gap_s);
             let mut cur_sentence: SentenceId = 0;
             let mut last_silence_feed = Instant::now(); // 断流喂静音的节流(100ms)
             let mut lead_in: VecDeque<Vec<i16>> = VecDeque::new(); // 起音补喂缓冲(~0.5s)
             let mut speech_active = false; // 上一帧 detected()——翻转时补喂 lead_in
             // 本句起音墙钟(rising edge 时刻)—— round26 量尺统一:settle 判定与
             // sentence.start_s 都用它,不再用 end−PCM 反推(PCM 含 0.5s lead-in、不含
             // 尾随 1s 静音,反推值偏晚 ~0.5s → 间隔虚增 → 与起音判定矛盾的
             // "同句中途换段" bug)。
             let mut onset_at: f64 = 0.0;
             // VAD 检测前端(front.rs,与 scout 采音同模块):喂帧 + detected 快照 +
             // 起音盲区门状态都在前端侧,本循环只消费它的输出。
             let mut vad_front = VadFront::new(Arc::clone(&self.mgr));

            loop {
                // ⓪ idle 深度睡眠:running=false → 退出消费循环。daemon 断开 scout,下一个客户端
                //   连接时置回 true 并重新调用 run() 恢复识别。
                if !self.running.load(Ordering::Relaxed) {
                    return;
                }
                // ① 连接开关:scout 暂停时挂起等音频,不做 VAD/ASR
                if !self.active.load(Ordering::Relaxed) {
                    let _ = wait_frame(&self.ring, &self.ring_notify, WINDOW, None).await;
                    continue;
                }

                // ② 时间驱动检查:主动归档 / 段落定稿 / 停滞看门狗 / 诊断
                let now_s = start.elapsed().as_secs_f64();
                // ②′ 冲刷流式任务回传:partial → 事件;镜像刷新(speaking/看门狗/断流判据)
                drain_stream_out(&mut stream_rx, &tracker, now_s, &mut mirror, on_event);
                // `speaking` 抑制段落按墙钟定稿——回溯式 VAD 的下一句 SOS 尚未到达,若
                // 定稿会把下一句错划进新段落。组合判定:partial 非空 **或** 起音盲区边际
                // 内(detected() 近期见过;见 VOICE_SETTLE_MARGIN)。
                let speaking = vad_front.speaking(mirror.nonempty, now_s);
                // 用户侧主动归档(IME 分字符 = "我说完了"):跳过 merge_gap 剩余等待立即整段
                // batch。说话中(EOS 未到)保持挂起下一 tick 重试 —— 立即切段会截断尾音;
                // 无段落则消费掉标记(空按,不让陈旧 flush 影响之后的语音)。
                if self.flush_paragraph.load(Ordering::Acquire) && !speaking {
                    match tracker.force_settle() {
                        Some(settled) => {
                            self.flush_paragraph.store(false, Ordering::Release);
                            info!(
                                paragraph_id = settled.paragraph_id,
                                sentences = settled.sentences.len(),
                                "flush: 主动归档(跳过 merge_gap 等待)"
                            );
                            emit_paragraph_edge(settled, &self.audio_store, sr, on_event);
                            let _ = stream_tx.send(StreamCmd::Reset); // 段落边界重置会话
                            mirror.nonempty = false;
                        }
                        None if !tracker.has_open_paragraph() => {
                            self.flush_paragraph.store(false, Ordering::Release);
                        }
                        None => {} // 句进行中 → 挂起,等 EOS 后下一 tick 强制定稿
                    }
                }
                if let Some(settled) = tracker.check_settle(now_s, speaking) {
                    emit_paragraph_edge(settled, &self.audio_store, sr, on_event);
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
                        ring = self.ring.lock().unwrap().len(),
                        has_partial,
                        "stage1 diag"
                    );
                    last_diag = Instant::now();
                }

                // ③ 取帧:ring 有帧直接取;空则 park 等音频/截止(断流>2s 且有 partial → 喂静音逼 EOS)
                let wake_at = next_wake_at(
                    &tracker,
                    mirror,
                    ring_empty_since,
                    now_s,
                    speaking,
                    self.flush_paragraph.load(Ordering::Acquire),
                );
                let frame = match self
                    .drain_frame(
                        &mut ring_empty_since,
                        mirror.nonempty,
                        &mut last_silence_feed,
                        wake_at,
                    )
                    .await
                {
                    FrameResult::Frame(f) => f,
                    FrameResult::Parked => continue,
                };
                frames_in += 1;

                // ④ VAD(front.rs 音频前端):每帧跑(便宜),得到 detected()(实时语音
                //    信号,门控流式)+ 分句事件;盲区门状态(speaking)在前端侧。
                let events = vad_front.feed(&frame, start.elapsed().as_secs_f64());
                let v_detected = vad_front.last_detected;

                // ⑤ 流式转发(VAD 门控;模型在独立任务):起音开段(A 侧 tracker)+
                //    补喂 lead_in(soft onset);语音帧经通道送 B accept+节流解码,partial
                //    回传由 ②′ 发射。accept 与 pcm 喂同一帧 → 流式/batch 共享音频。
                if has_stream {
                    if v_detected {
                        if !speech_active {
                            // ★ 起音即开段(§7-B 根治):rising edge 立刻分配真实段落 id ——
                            // 此后本段所有 partial/事件都携带真键,幽灵段(预测键)不复存在。
                            let at = start.elapsed().as_secs_f64();
                            tracker.on_speech_onset(at);
                            onset_at = at; // 本句起音墙钟(EOS 定稿的 settle 量尺)
                            // 起音:补喂 lead-in,让流式/batch 都听到 soft onset
                            let _ = stream_tx.send(StreamCmd::Onset {
                                lead_in: lead_in.drain(..).collect(),
                            });
                        }
                        let _ = stream_tx.send(StreamCmd::Feed(frame));
                    } else {
                        // 空闲:流式会话 park;只累积有界 lead-in(供下次起音补喂)
                        lead_in.push_back(frame);
                        if lead_in.len() > LEAD_IN_FRAMES {
                            lead_in.pop_front();
                        }
                    }
                    speech_active = v_detected;
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
                            self.finalize_sentence(
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
}

#[cfg(test)]
mod tests {
    use super::*;

mod tests {
    use super::*;

    fn sentence(id: SentenceId, start_s: f64, end_s: f64) -> VadSentence {
        VadSentence {
            id,
            audio_id: id,
            start_s,
            end_s,
            streaming_text: format!("s{id}"),
            batch_text: Some(format!("b{id}")),
        }
    }

    #[test]
    fn short_gap_absorbs_into_same_paragraph() {
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        let (settled, w1, sentences) = t.on_eos(sentence(s1, 0.0, 0.5));
        assert!(settled.is_none());
        assert_eq!(sentences.len(), 1);

        // gap 1.0−0.5 = 0.5 < 2.5 → same paragraph, second sentence (merge happens at EOS,
        // where the true onset is back-derived).
        let s2 = t.on_sos(0.0);
        let (settled, w, sentences) = t.on_eos(sentence(s2, 1.0, 1.5));
        assert!(settled.is_none(), "short gap must NOT settle");
        assert_eq!(w, w1, "same paragraph continues");
        assert_eq!(sentences.len(), 2, "both sentences in one paragraph");
    }

    #[test]
    fn big_gap_settles_previous_paragraph_and_opens_new_one() {
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // gap 5.0−0.5 = 4.5 ≥ 2.5 → settle w1 at the next sentence's EOS, open w2.
        let s2 = t.on_sos(0.0);
        let (settled, w2, sentences) = t.on_eos(sentence(s2, 5.0, 5.5));
        let s = settled.expect("big gap settles the previous paragraph");
        assert_eq!(s.paragraph_id, w1);
        assert_eq!(s.sentences.len(), 1);
        assert_ne!(w2, w1, "a fresh paragraph opens (random ids must differ)");
        assert_eq!(sentences.len(), 1);
    }

    #[test]
    fn settle_timeout_closes_trailing_paragraph() {
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        assert!(
            t.check_settle(2.0, false).is_none(),
            "2.0 − 0.5 = 1.5 < 2.5, not yet"
        );
        let s = t
            .check_settle(3.0, false)
            .expect("3.0 − 0.5 = 2.5 ≥ merge_gap → settle");
        assert_eq!(s.paragraph_id, w1);
        assert!(
            t.check_settle(10.0, false).is_none(),
            "nothing open anymore"
        );
    }

    #[test]
    fn force_settle_skips_merge_gap_wait() {
        // 主动归档:远未到 merge_gap 也能立即关段(IME"我说完了"信号)。
        let mut t = ParagraphTracker::new(2.5);
        assert!(
            t.force_settle().is_none(),
            "无段落 → None(调用方消费掉 flush 标记)"
        );
        assert!(!t.has_open_paragraph());
        let s1 = t.on_sos(0.0);
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // 0.2s 后强制归档(gap 0.2 < merge_gap 2.5 —— 常规定稿还早)。
        let s = t.force_settle().expect("有已定稿句 → 立即归档");
        assert_eq!(s.paragraph_id, w1);
        assert_eq!(s.sentences.len(), 1);
        assert!(!t.has_open_paragraph(), "段已关");
        assert!(
            t.check_settle(100.0, false).is_none(),
            "settle 路径不再重复触发"
        );
        // 归档后再次 force → 无段落 → None。
        assert!(t.force_settle().is_none());
    }

    #[test]
    fn force_settle_holds_while_sentence_active() {
        // 句进行中(SOS 已见 EOS 未到)→ 不动,调用方保持 flush 挂起。
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        let (_, _, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        let s2 = t.on_sos(0.0); // 第二句开口
        assert!(t.force_settle().is_none(), "active 句压制强制归档");
        assert!(t.has_open_paragraph(), "段落仍在 → flush 保持挂起");
        let (_, w, sentences) = t.on_eos(sentence(s2, 1.0, 1.5));
        let s = t.force_settle().expect("EOS 落定后重试成功");
        assert_eq!(s.paragraph_id, w);
        assert_eq!(sentences.len(), 2);
    }

    #[test]
    fn settle_deadline_counts_down_to_merge_gap() {
        // The condvar wake deadline: exactly when check_settle would fire (consumes loop
        // parks on the ring condvar instead of polling — this is its only wake source for
        // the trailing paragraph).
        let mut t = ParagraphTracker::new(2.5);
        assert!(t.settle_deadline(0.0, false).is_none(), "nothing open yet");
        let s1 = t.on_sos(0.0);
        t.on_eos(sentence(s1, 0.0, 0.5));
        assert!(
            (t.settle_deadline(1.0, false).unwrap() - 2.0).abs() < 1e-9,
            "2.5 − (1.0 − 0.5)"
        );
        assert!(
            (t.settle_deadline(3.0, false).unwrap() - 0.0).abs() < 1e-9,
            "due now, clamped at 0"
        );
        let _s2 = t.on_sos(0.0); // sentence in progress (active=true)
        assert!(
            t.settle_deadline(1.2, false).is_none(),
            "active sentence ⇒ suppressed, no deadline"
        );
    }

    #[test]
    fn active_sentence_suppresses_settle_timeout() {
        // Regression guard: a long following sentence must not be mistaken for "no
        // continuation" and force-split the paragraph mid-speech.
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        t.on_eos(sentence(s1, 0.0, 0.5));
        let _s2 = t.on_sos(0.0); // sentence in progress (active=true)
        assert!(
            t.check_settle(100.0, false).is_none(),
            "active sentence ⇒ settle suppressed"
        );
    }

    #[test]
    fn speaking_suppresses_settle_waiting_for_retroactive_sos() {
        // 回溯式 VAD 的回归防护:下一句的 SOS 要等它的 EOS 才到——在它到达前,流式
        // session 的 partial 非空(=speaking=true)必须抑制 settle 超时。否则墙钟超时
        // 会在下一句说话时定稿,把它错划进新段落(症状:段落永远只有 1 个 sentence)。
        let mut t = ParagraphTracker::new(2.5);
        let s1 = t.on_sos(0.0);
        t.on_eos(sentence(s1, 0.0, 0.5));
        // 下一句正在说话(SOS 尚未到),墙钟已远超 merge_gap —— speaking=true 抑制。
        assert!(
            t.check_settle(100.0, true).is_none(),
            "speaking ⇒ settle suppressed"
        );
        assert!(
            t.settle_deadline(100.0, true).is_none(),
            "speaking ⇒ no settle deadline"
        );
        // 说话停止(speaking=false)后,同一时刻立刻能定稿。
        assert!(
            t.check_settle(100.0, false).is_some(),
            "not speaking ⇒ settle fires"
        );
    }

    #[test]
    fn merge_gap_zero_makes_every_sentence_its_own_paragraph() {
        let mut t = ParagraphTracker::new(0.0);
        let s1 = t.on_sos(0.0);
        let (_, w1, _) = t.on_eos(sentence(s1, 0.0, 0.5));
        // Any gap ≥ 0 settles at the next sentence's EOS (gap 0.6 − 0.5 = 0.1 ≥ 0).
        let s2 = t.on_sos(0.6);
        let (settled, w2, _) = t.on_eos(sentence(s2, 0.6, 0.7));
        assert_eq!(settled.expect("gap 0.1 ≥ 0 settles").paragraph_id, w1);
        assert_ne!(w2, w1);
        // …and the settle timeout fires immediately after an EOS too.
        let s3 = t.on_sos(10.0);
        t.on_eos(sentence(s3, 10.0, 10.5));
        assert!(
            t.check_settle(10.5, false).is_some(),
            "now − end = 0 ≥ 0 → settle"
        );
    }

    // ── round13:起音即开段 + 时间戳 id(§7-A/B 修复)──────────────────────

    /// 起音开段 → prospective 返回**真实**段 id;该段后续所有事件(EOS 的
    /// Batch/ParagraphEdge)携带同一 id —— 幽灵段(预测键 ≠ 实际键)不复存在。
    #[test]
    fn onset_opens_paragraph_prospective_returns_real_id() {
        let mut t = ParagraphTracker::new(2.5);
        t.on_speech_onset(10.0);
        let (pid, _sid) = t.prospective();
        let s1 = t.on_sos(10.4);
        assert_eq!(t.prospective().0, pid, "段内 prospective 稳定");
        let (settled, w, _) = t.on_eos(sentence(s1, 10.0, 10.5));
        assert!(settled.is_none());
        assert_eq!(w, pid, "EOS 归属段 = 起音开的段(prospective 即真键)");
        // 静默满 merge_gap 关段,下一次起音 → 新段(时间戳更大)。
        let _ = t
            .check_settle(20.0, false)
            .expect("静默 9.5s ≥ 2.5s → settle");
        t.on_speech_onset(20.5);
        let (pid2, _) = t.prospective();
        assert!(pid2 > pid, "时间戳 id 严格递增 —— id 即顺序");
    }

    /// 时间戳 id 严格递增:同微秒连续开段(防御 max(last+1))也绝不重复/回退。
    #[test]
    fn timestamp_win_ids_strictly_increasing() {
        let mut t = ParagraphTracker::new(2.5);
        let mut prev = 0u64;
        for i in 0..8 {
            t.on_speech_onset(i as f64);
            let (pid, _) = t.prospective();
            assert!(pid > prev, "id 必须严格递增(时间戳,防时钟回拨/同微秒)");
            prev = pid;
            // 立刻出句并关段,下一轮开新段。
            let s = t.on_sos(i as f64);
            t.on_eos(sentence(s, i as f64, i as f64 + 0.5));
            let _ = t.check_settle(i as f64 + 10.0, false);
        }
    }

    /// 空段 GC:起音开的段从未出句(微弱音频)→ 静默满 merge_gap 静默丢弃;
    /// 不 GC 会让陈旧空段被很久之后的语音复用,id 落后 → 客户端排序错位。
    #[test]
    fn empty_onset_paragraph_gced_after_merge_gap() {
        let mut t = ParagraphTracker::new(2.5);
        t.on_speech_onset(0.0);
        let (pid, _) = t.prospective();
        assert!(t.check_settle(2.0, false).is_none(), "2.0 < 2.5,未到期");
        assert!(t.has_open_paragraph(), "GC 前段还在");
        assert!(
            t.check_settle(2.6, false).is_none(),
            "GC 静默:返回 None(无事件)"
        );
        assert!(!t.has_open_paragraph(), "空段静默满 merge_gap 即弃");
        // 下一次起音开**新**段(id 更大),不复用陈旧空段。
        t.on_speech_onset(100.0);
        let (pid2, _) = t.prospective();
        assert!(pid2 > pid, "新段时间戳更大");
        // settle_deadline 也覆盖空段(消费循环要能在 GC 时点醒来)。
        assert!(t.check_settle(103.0, false).is_none(), "GC 掉 100.0 的空段");
        assert!(!t.has_open_paragraph());
        t.on_speech_onset(200.0);
        let d = t.settle_deadline(201.0, false).expect("空段也有 GC 截止");
        assert!((d - 1.5).abs() < 1e-9, "2.5 − (201.0 − 200.0)");
    }

    /// 真语音不误伤:partial 非空(speaking)抑制空段 GC —— 长句(> merge_gap)
    /// 说话中不会被墙钟 GC 掉段落。
    #[test]
    fn speaking_suppresses_empty_onset_gc() {
        let mut t = ParagraphTracker::new(2.5);
        t.on_speech_onset(0.0);
        assert!(t.check_settle(100.0, true).is_none());
        assert!(t.has_open_paragraph(), "speaking ⇒ 空段不 GC");
        assert!(
            t.settle_deadline(100.0, true).is_none(),
            "speaking ⇒ 无 GC 截止"
        );
        assert!(t.check_settle(100.0, false).is_none(), "静默后 GC");
        assert!(!t.has_open_paragraph());
    }
}
}
