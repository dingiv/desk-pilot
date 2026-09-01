//! 服务层(service)—— 业务状态与动作。router 只调这里的 API,不直接碰持久化;
//! 识别事件 → 线协议的映射(含 Stage3 副作用)与 idle 深睡监控也住在这层。
//!
//! 双面协议:识别事件走**数据面**(`asr_events` broadcast → /api/asr_stream,
//! 低延迟直推);settings 变化 bump `version`(**控制面**,/api/stream 节流 ping,
//! 客户端重拉 /api/state)。识别事件不 bump version。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, Notify};
use tracing::info;

use audio_aura_agent::stage3_rule_trigger;
use audio_aura_agent::{AddHotwordTool, AsrEvent, AuraStateView, ConfigView, CorrectionView};
use audio_aura_core::archive::ClipMeta;
use audio_aura_core::{TurnEvent, TurnRecord};

use crate::repository::DataStore;

/// Shared daemon state surfaced over the socket(供 router 层 `State` 提取)。
#[derive(Clone)]
pub(crate) struct DaemonState {
    pub(crate) hotwords: Arc<Mutex<Vec<String>>>,
    pub(crate) corrections: Arc<Mutex<Vec<(String, String)>>>,
    /// Scout-connection toggle (shared with the Stage1 recognizer's ingest + run loop).
    pub(crate) active: Arc<AtomicBool>,
    /// 主动归档信号(IME 分字符)—— Stage1 消费循环消费;socket 端只置位。
    pub(crate) flush_paragraph: Arc<AtomicBool>,
    /// idle 深度睡眠信号: false → Stage1 退出 + 断开 scout; 恢复时置回 true。
    pub(crate) running: Arc<AtomicBool>,
    /// 当前是否处于 idle 深度睡眠。
    pub(crate) idle: Arc<AtomicBool>,
    /// 活跃的 SSE 长连接订阅数(数据面 + 控制面)。idle 监控据此判断"无客户端"。
    pub(crate) subscribers: Arc<AtomicUsize>,
    /// 恢复唤醒:pipeline 线程在 idle 后 park 在这里; 下一个客户端连接时 notify。
    /// 唤醒 pipeline 消费循环从深度睡眠恢复(round14b:异步 Notify,permit 语义)。
    pub(crate) resume_notify: Arc<Notify>,
    /// idle 深度睡眠超时; None = 关闭。
    pub(crate) idle_timeout: Option<Duration>,
    /// Bumped on ANY SETTINGS change (connected / hotword / correction). Recognition events do
    /// NOT bump — they're pushed via `asr_events` (the data plane). The SSE handler ticks at the
    /// client's rate and pings only when this advances.
    pub(crate) version: Arc<AtomicU64>,
    /// Data-plane broadcast: recognition sentences pushed directly to `GET /api/asr_stream`
    /// subscribers (low-latency, every event — unlike the throttled control-plane ping).
    pub(crate) asr_events: broadcast::Sender<AsrEvent>,
    pub(crate) config: ConfigView,
    pub(crate) stage3_on: bool,
    /// 持久化仓库(只读访问经下面的委托方法;落盘出口在 assemble 时交给 pipeline)。
    pub(crate) store: DataStore,
}

impl DaemonState {
    /// Assemble the full [`AuraStateView`] snapshot — lock each source, clone, release. Called by
    /// GET /api/state (every change). No lock is held across an await (clones are synchronous).
    pub(crate) fn snapshot(&self) -> AuraStateView {
        let hotwords = self.hotwords.lock().unwrap().clone();
        let corrections = self
            .corrections
            .lock()
            .unwrap()
            .iter()
            .map(|(r, c)| CorrectionView { raw: r.clone(), corrected: c.clone() })
            .collect();
        AuraStateView {
            connected: self.active.load(Ordering::Relaxed),
            stage3_on: self.stage3_on,
            config: self.config.clone(),
            hotwords,
            corrections,
        }
    }

    /// Signal that state changed — the SSE handler's next eligible tick will ping clients.
    pub(crate) fn bump(&self) {
        self.version.fetch_add(1, Ordering::Release);
    }

    /// 恢复识别:置 running=true + active=true(重连 scout), 唤醒 pipeline 线程重跑消费循环。
    pub(crate) fn resume(&self) {
        self.active.store(true, Ordering::Release);
        self.idle.store(false, Ordering::Release);
        if self.running.swap(true, Ordering::Release) == false {
            info!("client connected — resuming recognition");
        }
        self.resume_notify.notify_one();
    }

    /// 进入 idle 深度睡眠:running=false(Stage1 消费循环退出) + active=false(断开 scout)。
    pub(crate) fn enter_idle(&self) {
        if self.running.load(Ordering::Acquire) == true {
            info!("entering idle — no subscribers, disconnecting scout");
            self.running.store(false, Ordering::Release);
            self.active.store(false, Ordering::Release);
            self.idle.store(true, Ordering::Release);
        }
    }

    /// Scout 开关:无参 = 翻转,有参 = 置位。connected 属 settings → bump 控制面。
    /// 返回生效后的值。
    pub(crate) fn set_scout(&self, enabled: Option<bool>) -> bool {
        let next = match enabled {
            Some(v) => v,
            None => !self.active.load(Ordering::Relaxed),
        };
        self.active.store(next, Ordering::Relaxed);
        self.bump(); // connected changed → ping clients to re-fetch
        next
    }

    /// 主动归档当前开放段落(IME 分字符 `'` 触发):置位即返,Stage1 消费循环
    /// (≤50ms 唤醒)消费标记并立即整段 batch。识别域动作,不 bump version ——
    /// 归档产生的段落事件走数据面 /api/asr_stream 推送。
    pub(crate) fn request_flush(&self) {
        self.flush_paragraph.store(true, Ordering::Release);
    }

    /// 记录用户纠正:push 进 Stage2 纠正环(cap 20)+ 数据面广播 Correction 事件
    /// (标记时间线条目)+ bump 控制面(快照里的 corrections 变了)。
    /// 校验失败(raw/corrected 任一为空)返回 false,调用方回错误 JSON。
    pub(crate) fn add_correction(&self, raw: &str, corrected: &str, paragraph_id: u64) -> bool {
        if raw.is_empty() || corrected.is_empty() {
            return false;
        }
        // Push to correction store (ring buffer, cap 20 — short-term memory for Stage2)
        {
            let mut c = self.corrections.lock().unwrap();
            if c.len() >= 20 {
                c.remove(0);
            } // evict oldest
            c.push((raw.to_string(), corrected.to_string()));
        }
        // Data plane: tell subscribers to mark the paragraph corrected (the live list is
        // client-side).
        let _ = self.asr_events.send(AsrEvent::Correction {
            paragraph_id,
            raw: raw.to_string(),
            corrected: corrected.to_string(),
        });
        // Control plane: the corrections list changed → re-fetch snapshot.
        self.bump();
        info!("user correction added → Stage2");
        true
    }

    // ── 持久化只读委托(router 层不直接摸 repository)──

    /// 段落 WAV 回放字节(hot tier 优先,磁盘文件兜底)。
    pub(crate) fn wav(&self, paragraph_id: u64) -> Option<Vec<u8>> {
        self.store.wav(paragraph_id)
    }

    /// 全部已知 clip 列表(hot + flushed,seq 升序)。
    pub(crate) fn recordings(&self) -> Vec<ClipMeta> {
        self.store.recordings()
    }

    /// 最近定稿 turn(最旧 → 最新)。
    pub(crate) fn recent_turns(&self) -> Vec<TurnRecord> {
        self.store.recent()
    }
}

/// TurnEvent → AsrEvent 线协议映射(pipeline 回调出口)。Stage3 加热词属 settings
/// 变化 → bump 控制面;识别事件本体只回 Some(数据面直推)。
pub(crate) fn turn_to_wire(
    stage3_on: bool,
    tool: &AddHotwordTool,
    version: &AtomicU64,
    ev: TurnEvent,
) -> Option<AsrEvent> {
    match ev {
        TurnEvent::StreamFragment { paragraph_id, sentence_id, text, at_s } => {
            Some(AsrEvent::StreamFragment {
                paragraph_id,
                sentence_id,
                text: text.to_string(),
                at_s,
            })
        }
        // 段落边界:server 保证的时序信号 —— 必先于下一段的任何事件
        // (pipeline 主循环按序直发,round11 S3)。
        TurnEvent::ParagraphClosed { paragraph_id } => Some(AsrEvent::ParagraphClosed { paragraph_id }),
        TurnEvent::BatchSentence { paragraph_id, sentence_id, text } => {
            Some(AsrEvent::BatchSentence { paragraph_id, sentence_id, text })
        }
        TurnEvent::BatchParagraph { paragraph_id, text } => {
            Some(AsrEvent::BatchParagraph { paragraph_id, text })
        }
        TurnEvent::SentenceCalibration { paragraph_id, sentence_id, calibrated, .. } => {
            Some(AsrEvent::SentenceCalibration { paragraph_id, sentence_id, calibrated })
        }
        TurnEvent::ParagraphCalibration { paragraph_id, calibrated, route_ms } => {
            // Stage3 may add hotwords — that's a SETTINGS change → control plane.
            if stage3_on && stage3_rule_trigger(tool, &calibrated) {
                version.fetch_add(1, Ordering::Release);
            }
            let _ = route_ms;
            Some(AsrEvent::ParagraphCalibration { paragraph_id, calibrated })
        }
    }
}

/// idle 深度睡眠监控:无 SSE 订阅持续 `idle_timeout` → enter_idle(Stage1 退出 +
/// 断开 scout)。30s 巡检一次;有订阅随时清零计时。
pub(crate) fn spawn_idle_monitor(rt: &tokio::runtime::Runtime, state: DaemonState) {
    if let Some(timeout) = state.idle_timeout {
        if timeout > Duration::ZERO {
            rt.spawn(async move {
                let mut since: Option<Instant> = None;
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if state.subscribers.load(Ordering::Relaxed) == 0 {
                        match since {
                            None => since = Some(Instant::now()),
                            Some(t) if t.elapsed() >= timeout => {
                                state.enter_idle();
                                since = None;
                            }
                            Some(_) => {}
                        }
                    } else {
                        since = None;
                    }
                }
            });
        }
    }
}
