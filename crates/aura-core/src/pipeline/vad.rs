//! vad — Stage0 检测引擎(可换缝):[`Stage0VAD`] trait + [`SileroVAD`] 默认实现。
//! 引擎只回答"这一帧有没有人说话 + 分句事件"(回溯式 SOS/EOS);门控/直发/队列/
//! 断流静音在 [`crate::pipeline::front`](拉流线程),settle 抑制胶水
//! [`speech_pending`] 也在 front(它知道 partial —— ASR 概念不进引擎)。
//! 换 VAD(webrtc/energy/远端)= 加一个 impl,零改其余文件。

use std::sync::{Arc, Mutex};

use crate::VadEvent;
use dp_models::onnx::OnnxRuntimeManager;

/// Stage0 检测器:帧 → "有没有人在说话 + 分句边界"。与 Stage1(ASR)物理隔离 ——
/// 不碰识别、不知道流式 partial 的存在(settle 抑制胶水在 [`speech_pending`])。
/// `&self` + 内部可变:骑 `Arc` 双线程共享(R2 起前端线程写、消费循环读快照);
/// 单逻辑写者 = 拉流线程,31.25Hz 无争用。
pub trait Stage0VAD: Send + Sync {
    /// 喂一帧(32ms):跑检测,返回回溯式分句事件(SOS/EOS 同批、EOS 携句 PCM)。
    fn feed(&self, frame: &[i16], now_s: f64) -> Vec<VadEvent>;
    /// 最近一次 feed 后的 detected() 快照(门控流式转发用)。
    fn detected(&self) -> bool;
    /// 最近一次 detected=true 的墙钟(-1 = 从未见语音;与消费循环同一
    /// `start: Instant` 原点的秒值)—— settle 抑制的起音盲区边际用。
    fn last_voice_at(&self) -> f64;
}

/// [`Stage0VAD`] 默认实现:Silero(dp-models ONNX,经 `OnnxRuntimeManager`),
/// 吸收原 VadFront。
pub struct SileroVAD {
    mgr: Arc<OnnxRuntimeManager>,
    state: Mutex<VadState>,
}

struct VadState {
    last_detected: bool,
    last_voice_s: f64,
}

impl SileroVAD {
    pub fn new(mgr: Arc<OnnxRuntimeManager>) -> Self {
        Self {
            mgr,
            state: Mutex::new(VadState { last_detected: false, last_voice_s: -1.0 }),
        }
    }
}

impl Stage0VAD for SileroVAD {
    /// 喂一帧(32ms):跑 VAD(便宜),记 detected/last_voice,返回分句事件
    /// (sherpa 的 SOS/EOS 是**回溯式**——与 EOS 同批到达,携带句 PCM)。
    fn feed(&self, frame: &[i16], now_s: f64) -> Vec<VadEvent> {
        let vad = self.mgr.vad().expect("Silero VAD loaded at startup");
        let events = vad.push_frame(frame);
        let detected = vad.detected();
        let mut st = self.state.lock().unwrap();
        st.last_detected = detected;
        if detected {
            st.last_voice_s = now_s;
        }
        events
    }

    fn detected(&self) -> bool {
        self.state.lock().unwrap().last_detected
    }

    fn last_voice_at(&self) -> f64 {
        self.state.lock().unwrap().last_voice_s
    }
}
