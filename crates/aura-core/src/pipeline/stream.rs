//! stream — 流式识别任务(round21):accept/decode(ONNX 前向)独立于消费循环,
//! `run_stream_worker` 是 async fn 本体,由消费循环 `tokio::spawn` 交 executor 协作
//! 调度(不占阻塞线程)。消费循环只转发帧指令(`Onset`/`Feed`/`Reset`/`Finalize`),
//! partial 回传后仍由消费循环发射(两任务汇于同一事件出口,SF→BS→PC/PCal 全序不破)。
//! EOS 定稿 = 每句一次、回执同通道(唯一同步点,B 侧本地 finalize,几十 ms)。
//! B 侧 last_partial 状态以 [`PartialMirror`] 镜像进消费循环,供 speaking 抑制 /
//! 断流喂静音 / 停滞看门狗;重置/定稿点由消费循环直接清零(确定性,无竞态)。

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc as t_mpsc;

use dp_models::onnx::{OnnxRuntimeManager, StreamingSession};

use crate::pipeline::tracker::ParagraphTracker;
use crate::Stage1Event;

/// Streaming-partial decode cadence: every N paragraphs (~0.3s @ 32ms Silero paragraphs).
const PARTIAL_EVERY_FRAMES: u32 = 9;
/// Stale-session watchdog: reset the streaming session when its partial has been UNCHANGED
/// this long AND no EOS came — that means VAD never latched (audio below `threshold` =
/// discard-by-design), and its residue (hallucinated repetitions included) must NOT leak
/// into whatever sentence closes next (2026-08-17 实测:35s 悬置会话把上一句幻觉文本卷进
/// 下一句). Real speech never trips this: a ≥min_silence pause closes the sentence via EOS,
/// which resets the session long before the partial could go stale.
pub(crate) const STALE_SESSION_RESET: std::time::Duration = std::time::Duration::from_secs(8);
/// The live streaming session + its partial-throttle state — **owned by the dedicated
/// streaming task** ([`run_stream_worker`]). D1 adaptation: sherpa's VAD emits SOS
/// RETROACTIVELY (together with EOS — the sentence only pops complete), so the session
/// CANNOT be created at speech onset. Instead it is fed CONTINUOUSLY and RESET at every
/// sentence boundary (EOS) and paragraph settle — each session therefore covers exactly
/// [previous boundary, this EOS] ≈ this one sentence (+ surrounding silence, which decodes
/// to nothing). Per-sentence attribution is preserved; live partials keep flowing.
struct ActiveSession {
    stream: StreamingSession,
    frames_since_partial: u32,
    last_partial: String,
    /// Diagnostic: frames fed since the last reset.
    fed: u32,
    /// Every fed frame, accumulated — the EXACT audio this streaming session heard. At EOS this
    /// becomes the sentence's PCM (shared with the batch ASR), so streaming and batch see the
    /// same audio — including the soft onset BEFORE VAD's threshold crossing, which the VAD's
    /// own sentence cuts off (the "batch drops the first 2-3 chars" bug). Bounded by the sentence
    /// length (+ boundary silence), reset at every EOS / paragraph settle.
    pcm: Vec<i16>,
}

impl ActiveSession {
    fn new(stream: StreamingSession) -> Self {
        Self {
            stream,
            frames_since_partial: 0,
            last_partial: String::new(),
            fed: 0,
            pcm: Vec::new(),
        }
    }
}

// ── round21:流式模型独立任务 ──────────────────────────────────────────────
// VAD 循环与流式解码彻底分任务:accept_waveform / decode_and_result(ONNX 前向,CPU
// 密集)不再与 VAD/分句/段落定稿共享执行流。帧经无界通道转发(音频速率 31 msg/s,
// B 处理快于实时,不积压);partial 回传后仍由消费循环发射 —— 两任务汇于同一事件
// 出口,顺序不变式(SF…→BS→PC/PCal)不破。唯一同步点:EOS 定稿(每句一次,
// 回执走同一条 out 通道的 `Finalized` 变体 —— round24 起不再有 per-句 oneshot,
// 整个任务只有一对通道)。

/// VAD 循环 → 流式任务指令。
pub(crate) enum StreamCmd {
    /// 起音(rising edge):补喂 lead-in(soft onset 进会话),重置解码节拍。
    Onset { lead_in: Vec<Vec<i16>> },
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

/// B 侧 last_partial 状态的消费循环镜像(speaking 抑制 / 断流喂静音判据 / 停滞看门狗):
/// 每次 B 回传刷新;重置/定稿点由本侧直接清零(确定性,无竞态)。
#[derive(Clone, Copy)]
pub(crate) struct PartialMirror {
    pub(crate) nonempty: bool,
    pub(crate) last_change: Instant,
}

impl PartialMirror {
    pub(crate) fn empty() -> Self {
        Self {
            nonempty: false,
            last_change: Instant::now(),
        }
    }
}

/// 流式识别任务**本体**(round21:async fn)。由消费循环侧 `tokio::spawn` 交出去 ——
/// executor 协作调度(ONNX 前向几十 ms 量级,标准协作负载),**不占阻塞线程**。
/// cmd sender drop(消费循环退出)即任务结束。
pub(crate) async fn run_stream_worker(
    mgr: Arc<OnnxRuntimeManager>,
    sr: u32,
    mut cmd_rx: t_mpsc::UnboundedReceiver<StreamCmd>,
    out_tx: t_mpsc::UnboundedSender<StreamOut>,
) {
    let Some(asr) = mgr.streaming_asr() else {
        // 流式未配置:与旧内联行为一致——全部 no-op;finalize 回空定稿
        // (pcm: None → 调用方 fallback VAD 句)。
        while let Some(cmd) = cmd_rx.recv().await {
            if matches!(cmd, StreamCmd::Finalize) {
                let _ = out_tx.send(StreamOut::Finalized(Box::new(StreamFinal {
                    text: String::new(),
                    pcm: None,
                    fed: 0,
                })));
            }
        }
        return;
    };
    let mut a = ActiveSession::new(asr.create_session());
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            StreamCmd::Onset { lead_in } => {
                for chunk in &lead_in {
                    a.stream.accept_waveform(sr as i32, chunk);
                    a.pcm.extend_from_slice(chunk);
                    a.fed += 1;
                }
                a.frames_since_partial = 0; // 补喂后重新起解码节拍
            }
            StreamCmd::Feed(f) => {
                a.stream.accept_waveform(sr as i32, &f);
                a.pcm.extend_from_slice(&f); // 流式与 batch 共用同一句音频
                a.fed += 1;
                a.frames_since_partial += 1;
                if a.frames_since_partial >= PARTIAL_EVERY_FRAMES {
                    let partial = asr.decode_and_result(&a.stream);
                    let changed = !partial.is_empty() && partial != a.last_partial;
                    if changed {
                        a.last_partial = partial.clone();
                    }
                    let _ = out_tx.send(StreamOut::Partial {
                        text: changed.then_some(partial),
                        nonempty: !a.last_partial.is_empty(),
                    });
                    a.frames_since_partial = 0;
                }
            }
            StreamCmd::Reset => a = ActiveSession::new(asr.create_session()),
            StreamCmd::Finalize => {
                let text = asr.finalize_and_result(&a.stream);
                let fin = StreamFinal {
                    text,
                    pcm: Some(std::mem::take(&mut a.pcm)),
                    fed: a.fed,
                };
                // 回执失败 = 循环已退出,会话随之丢弃
                let _ = out_tx.send(StreamOut::Finalized(Box::new(fin)));
                a = ActiveSession::new(asr.create_session());
            }
        }
    }
}

/// 冲刷流式任务回传:`text` 变化 → 发射 `StreamFragment`;镜像刷新(speaking 抑制 /
/// 停滞看门狗 / 断流喂静音判据)。partial 变化时刻 = 镜像刷新时刻(与旧内联语义一致)。
/// 折叠一条回传:partial → 发射 SF + 镜像刷新(drain 与 await_finalize 共用)。
fn fold_stream_out(
    out: StreamOut,
    tracker: &ParagraphTracker,
    at_s: f64,
    mirror: &mut PartialMirror,
    on_event: &mut dyn FnMut(Stage1Event),
) {
    if let StreamOut::Partial { text, nonempty } = out {
        if text.is_some() {
            mirror.last_change = Instant::now(); // partial 变化时刻 = 镜像刷新时刻
        }
        if let Some(text) = text {
            let (paragraph_id, sentence_id) = tracker.prospective();
            on_event(Stage1Event::StreamFragment {
                paragraph_id,
                sentence_id,
                text,
                at_s,
            });
        }
        mirror.nonempty = nonempty;
    }
    // Finalized 由调用方(await_finalize)处理,不会出现在这里。
}

/// 冲刷在途回传(非阻塞):partial → 事件 + 镜像刷新。
pub(crate) fn drain_stream_out(
    stream_rx: &mut t_mpsc::UnboundedReceiver<StreamOut>,
    tracker: &ParagraphTracker,
    at_s: f64,
    mirror: &mut PartialMirror,
    on_event: &mut dyn FnMut(Stage1Event),
) {
    while let Ok(out) = stream_rx.try_recv() {
        fold_stream_out(out, tracker, at_s, mirror, on_event);
    }
}

/// EOS 定稿等待(round24:回执走同一条 out 通道,不再有 per-句 oneshot):
/// 挂起 recv 直至 `Finalized` —— 途中 partial 依序发射(FIFO 保证 partial 先于回执)。
/// 通道关闭(流式任务已亡)= 空定稿(调用方 fallback VAD 句)。
pub(crate) async fn await_finalize(
    stream_rx: &mut t_mpsc::UnboundedReceiver<StreamOut>,
    tracker: &ParagraphTracker,
    at_s: f64,
    mirror: &mut PartialMirror,
    on_event: &mut (dyn FnMut(Stage1Event) + Send),
) -> StreamFinal {
    while let Some(out) = stream_rx.recv().await {
        if let StreamOut::Finalized(fin) = out {
            return *fin;
        }
        fold_stream_out(out, tracker, at_s, mirror, on_event);
    }
    StreamFinal { text: String::new(), pcm: None, fed: 0 }
}

