//! vad — 音频前端(scout 采音 + VAD 检测;round24 起随流水线环节入 pipeline/):
//! - [`ingest_loop`]:omni-scout `/audio`(TCP)→ [`AudioRing`] 的阻塞采音循环
//!   (自动重连;由 Pipeline 在 blocking 桥上运行);
//! - [`VadFront`]:Silero VAD 喂帧(`detected()` 快照 + 回溯式 SOS/EOS 事件)+
//!   起音盲区门(`speaking`)。
//!
//! 与后端(流式 ASR 任务 `stream.rs`、batch ASR)物理分离:前端只产出"有没有人在
//! 说话 + 分句边界 + 音频帧",不碰识别。

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::buffer::AudioRing;
use crate::scout::ScoutAudioSource;
use crate::VadEvent;
use dp_models::onnx::{OnnxRuntimeManager, WINDOW};

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

/// VAD 检测前端:喂帧(Silero,`&self` 内部可变,引擎在 dp-models 的 mgr 里)+
/// `detected()` 快照 + 起音盲区门。**与 scout 采音同模块** —— 前端产出"有没有人在
/// 说话 + 分句边界",编排层(recognizer)只消费它的输出。
pub(crate) struct VadFront {
    mgr: Arc<OnnxRuntimeManager>,
    /// 最近一帧 detected()=true 的墙钟(初始 -1 = 从未有语音)—— settle 抑制的
    /// 起音盲区边际用。
    last_voice_s: f64,
    /// 最近一次 feed 后的 detected() 快照(门控流式转发用)。
    pub(crate) last_detected: bool,
}

impl VadFront {
    pub(crate) fn new(mgr: Arc<OnnxRuntimeManager>) -> Self {
        Self { mgr, last_voice_s: -1.0, last_detected: false }
    }

    /// 喂一帧(32ms):跑 VAD(便宜),记 detected/last_voice,返回分句事件
    /// (sherpa 的 SOS/EOS 是**回溯式**——与 EOS 同批到达,携带句 PCM)。
    pub(crate) fn feed(&mut self, frame: &[i16], now_s: f64) -> Vec<VadEvent> {
        let vad = self.mgr.vad().expect("Silero VAD loaded at startup");
        let events = vad.push_frame(frame);
        self.last_detected = vad.detected();
        if self.last_detected {
            self.last_voice_s = now_s;
        }
        events
    }

    /// settle 抑制的"说话中"判定(见 [`speech_pending`]):partial 非空,或起音
    /// 盲区边际内(detected 近期见过)。
    pub(crate) fn speaking(&self, partial_nonempty: bool, now_s: f64) -> bool {
        speech_pending(partial_nonempty, self.last_voice_s, now_s)
    }
}

/// 阻塞采音循环:omni-scout `/audio`(TCP)→ [`AudioRing`],push 后 `notify_one`
/// 唤醒异步消费循环(截止驱动,无轮询;notify 可从同步代码调用)。自动重连
/// (2s backoff),`active=false` 暂停连接。**Blocking —— Pipeline 在 blocking 桥上
/// 运行**,本 crate 不自行起线程。
pub(crate) fn ingest_loop(
    scout_addr: String,
    chunk_ms: Option<u64>,
    ring: Arc<Mutex<AudioRing>>,
    notify: Arc<Notify>,
    active: Arc<AtomicBool>,
) -> ! {
    let src = ScoutAudioSource::with_active(scout_addr, WINDOW, active).with_chunk_ms(chunk_ms);
    src.stream(
        move |win| {
            let mut g = ring.lock().unwrap();
            g.push(win);
            drop(g);
            notify.notify_one();
        },
        Duration::from_secs(2),
    )
}
