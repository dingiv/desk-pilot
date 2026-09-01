//! front — Stage0 拉流线程(音频前端本体,round27 从 vad.rs 拆出):scout TCP
//! 阻塞读 → 重切 32ms 窗 → `Stage0VAD::feed`(引擎在 [`crate::pipeline::vad`])
//! → 门控帧直发流式(Onset/Feed)+ FrontEvent 入队唤醒大脑;断流 >2s 且大脑报
//! partial 非空 → 本线程喂合成静音逼 EOS。**Blocking —— Pipeline 在 blocking 桥
//! 上运行**,本 crate 不自行起线程。时钟:与消费循环同一原点(施工案 D9)。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;


use crate::pipeline::types::{FrontBridge, FrontEvent, StreamCmd, FRONT_Q_CAP};
use crate::scout::ScoutAudioSource;
use dp_models::onnx::WINDOW;

/// 起音→首条 partial 的盲区边际:partial 每 9 帧(~0.3s)才解码一次,起音后这段
/// 盲区里 `partial 非空` 还没翻转,但 VAD `detected()` 已经是 true —— settle 判定若
/// 只看 partial,起音落在 merge_gap 截止点前盲区里的下一句会被**误切**(段落本该
/// 合并;且关段后仍产生该段的 SF,客户端首选回落陈旧流式 = "batch 后退回流式"
/// 的 round15 回归)。0.6s = 0.3s 节流 + 起音补喂/解码余量。
const VOICE_SETTLE_MARGIN: f64 = 0.6;

/// settle 抑制的"说话中"判定:partial 非空,**或**最近一帧 VAD detected() 距今
/// < [`VOICE_SETTLE_MARGIN`]。
pub(crate) fn speech_pending(partial_nonempty: bool, last_voice_s: f64, now_s: f64) -> bool {
    partial_nonempty || (now_s - last_voice_s) < VOICE_SETTLE_MARGIN
}

/// VAD 门控流式的 lead-in 帧数(每帧 32ms):detected() 翻转起音时补喂最近 ~0.5s
/// 的帧,让 soft onset 进入流式(Silero 要几帧过阈值,detected 翻转晚于真实起音)。
/// (R3 起门控与补喂住拉流线程。)
pub(crate) const LEAD_IN_FRAMES: usize = 16;

/// 阻塞采音+检测循环(R2:VAD 下沉——逐帧检测躲不掉,与采音同居拉流线程):
/// omni-scout `/audio`(TCP)→ 重切 32ms 窗(原 AudioRing 职责)→ [`Stage0VAD::feed`]
/// → FrontEvent 入队(有界,满丢最旧)→ `notify_one` 唤醒异步消费循环(截止驱动,
/// 无轮询;notify 可从同步代码调用)。自动重连(2s backoff),`active=false` 暂停
/// 连接。**Blocking —— Pipeline 在 blocking 桥上运行**,本 crate 不自行起线程。
/// 时钟:`start` 与消费循环同一原点(施工案 D9,一把量尺)。
pub(crate) fn ingest_loop(scout_addr: String, chunk_ms: Option<u64>, b: FrontBridge) -> ! {
    let FrontBridge { vad, has_stream, stream_port, partial_live, start, front_q, notify, active } = b;
    let src = ScoutAudioSource::with_active(scout_addr, WINDOW, active).with_chunk_ms(chunk_ms);
    // scout 推任意大小 chunk → 累积重切成 WINDOW 帧再逐帧喂 VAD。
    let mut pending: VecDeque<i16> = VecDeque::with_capacity(WINDOW * 2);
    // 门控状态(R3 起住前端):上一帧 detected(翻转补喂 lead_in)+ 起音缓冲。
    let mut speech_active = false;
    let mut lead_in: VecDeque<Vec<i16>> = VecDeque::new();
    // 断流喂静音状态(R4 住前端):(last_data, last_silence_feed),两闭包共享。
    let starve: Arc<Mutex<(std::time::Instant, std::time::Instant)>> =
        Arc::new(Mutex::new((std::time::Instant::now(), std::time::Instant::now())));
    let starve_data = Arc::clone(&starve);
    let starve_feed = Arc::clone(&starve);
    let vad_s = Arc::clone(&vad);
    let front_q_s = Arc::clone(&front_q);
    let notify_s = Arc::clone(&notify);
    src.stream_with_starve(
        move |chunk| {
            starve_data.lock().unwrap().0 = std::time::Instant::now();
            pending.extend(chunk);
            while pending.len() >= WINDOW {
                let frame: Vec<i16> = (0..WINDOW).filter_map(|_| pending.pop_front()).collect();
                let events = vad.feed(&frame, start.elapsed().as_secs_f64());
                let detected = vad.detected();
                // 门控转发(R3):前端刚算完 detected,直接决定帧去向 —— 样本只进
                // 流式(batch 吃 EOS 定稿的整句 PCM,大脑经 Finalize 握手交接)。
                let mut onset = None;
                if has_stream {
                    let port = stream_port.lock().unwrap().clone();
                    if let Some(tx) = port {
                        if detected {
                            if !speech_active {
                                // ★ 起音即开段(量尺 D9):onset 墙钟随 FrontEvent
                                //    给大脑,lead_in 补喂走 Onset 指令。
                                onset = Some(start.elapsed().as_secs_f64());
                                let _ = tx.send(StreamCmd::Onset {
                                    at: onset.unwrap(),
                                    lead_in: lead_in.drain(..).collect(),
                                });
                            }
                            let _ = tx.send(StreamCmd::Feed(frame));
                        } else {
                            // 空闲:流式会话 park;只累积有界 lead-in(供下次起音补喂)
                            lead_in.push_back(frame);
                            if lead_in.len() > LEAD_IN_FRAMES {
                                lead_in.pop_front();
                            }
                        }
                        speech_active = detected;
                    } else {
                        // idle 深睡(port=None):不转发;门控状态清零,run() 重入
                        // 等价全新开始(与旧实现 per-run 局部 speech_active 一致)。
                        speech_active = false;
                        lead_in.clear();
                    }
                }
                let mut g = front_q.lock().unwrap();
                if g.len() >= FRONT_Q_CAP {
                    g.pop_front(); // 满丢最旧 = 环回
                }
                g.push_back(FrontEvent { detected, events, onset });
                drop(g);
                notify.notify_one();
            }
        },
        move || {
            // 断流喂静音(R4,VAD 需静音帧才吐 EOS):距上次数据 >2s 且大脑报
            // partial 非空 → 每 ~100ms 合成一帧静音只喂 VAD(不进流式/lead_in);
            // 有事件产出(EOS)才入队唤醒大脑(detected=false/onset=None 无信息量)。
            let mut g = starve_feed.lock().unwrap();
            if g.0.elapsed() > Duration::from_secs(2)
                && partial_live.load(std::sync::atomic::Ordering::Acquire)
                && g.1.elapsed() >= Duration::from_millis(100)
            {
                g.1 = std::time::Instant::now();
                drop(g);
                let events = vad_s.feed(&[0i16; WINDOW], start.elapsed().as_secs_f64());
                if !events.is_empty() {
                    front_q_s
                        .lock()
                        .unwrap()
                        .push_back(FrontEvent { detected: false, events, onset: None });
                    notify_s.notify_one();
                }
            }
        },
        Duration::from_secs(2),
    )
}
