//! agent.rs — `AuraAgent`: the managed-state facade over [`AuraClient`]. A client (swift-ime,
//! geek-familiar, …) imports this crate, constructs the agent with [`AuraAgent::new`], then
//! hands it an external tokio [`Handle`] via [`AuraAgent::start`] — the agent does NOT own a
//! runtime or driver thread, the caller does.
//!
//! - **Control plane**: the latest [`AuraStateView`] snapshot (re-fetched on each `state_changed`
//!   ping), readable synchronously via [`AuraAgent::state`].
//! - **Data plane**: the live recognition text ([`AuraAgent::live`]) and the accumulated
//!   window list ([`AuraAgent::windows`], with corrected flags), built from the
//!   `AsrSegment` stream (boundary paradigm — windows settle one per merge gap).
//! - **Connectivity**: a periodic `/health` probe ([`AuraAgent::conn`]).
//!
//! Every change also surfaces as an [`AgentEvent`] on [`AuraAgent::events`] for clients that want
//! push semantics (e.g. a UI that updates on each StreamFragment / WindowCalibration). Commands
//! (`set_connected`, `correct`, `audio`) are async — call them from the same runtime you handed
//! to `start`, or use the [`AuraAgent::poll_events`] sync drain for legacy loops.
//!
//! ```ignore
//! use audio_aura_agent::agent::AuraAgent;
//! let agent = AuraAgent::new("http://127.0.0.1:9091")?;
//! agent.start(&runtime.handle()).await;
//! let wins = agent.windows();          // sync read, never blocks
//! agent.correct(3, "蛇声", "蛇身").await; // 异步命令 — 由调用方在外部 runtime 上 await
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use futures::{Stream, StreamExt};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::client::AuraClient;
use crate::view::{AsrSegment, AuraStateView};

/// Health-probe cadence (the segment stream alone can't tell "connected but silent" from
/// "reconnecting" during pauses).
const HEALTH_PROBE: Duration = Duration::from_secs(3);
/// `state_changed` ping floor (ms) — the server clamps to ≥250ms anyway.
const STATE_FREQ_MS: u64 = 250;
/// Bounded event queue (drop-oldest when a slow client can't keep up; state stays fresh in the
/// RwLocks regardless).
const EVENT_CAP: usize = 256;

/// Connectivity, best-effort (last known).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuraConn {
    /// No health reply yet / first connect in flight.
    Connecting = 0,
    /// Last `/health` ok OR a segment just arrived.
    Connected = 1,
    /// `/health` failed (daemon down / unreachable).
    Disconnected = 2,
}

impl AuraConn {
    fn from_ok(ok: bool) -> Self {
        if ok { AuraConn::Connected } else { AuraConn::Disconnected }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            1 => AuraConn::Connected,
            2 => AuraConn::Disconnected,
            _ => AuraConn::Connecting,
        }
    }
}

/// One settled window, as the agent maintains it from the data plane (`WindowCalibration` +
/// `Correction` segments). The snapshot carries no windows — this list is the client's
/// transcript source. The raw/streaming layers live in the `batch_window` / `stream_fragment`
/// events themselves, so the settled record only needs the final calibrated text.
#[derive(Debug, Clone)]
pub struct WindowView {
    pub window_id: u64,
    pub calibrated: String,
    /// True once a `Correction` segment for this window arrived (the pair also enters Stage2).
    pub corrected: bool,
}

/// Everything the agent surfaces to the client — read via [`AuraAgent::events`]. Five
/// recognition events mirroring the daemon's `AsrSegment` variants (boundary paradigm):
/// ① [`StreamFragment`](AgentEvent::StreamFragment) — a segment's raw streaming output;
/// ② [`BatchSegment`](AgentEvent::BatchSegment) — a segment's batch pass; ③
/// [`BatchWindow`](AgentEvent::BatchWindow) — the whole-window batch re-run;
/// ④ [`SegmentCalibration`](AgentEvent::SegmentCalibration) — Stage2's provisional JOINT
/// calibration of the current window (one per VAD gap, replaces the window's previous
/// calibration); ⑤ [`WindowCalibration`](AgentEvent::WindowCalibration) — the merge window
/// closed (authoritative).
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Connectivity changed (reconnecting / connected / lost).
    ConnChanged(AuraConn),
    /// The control-plane snapshot refreshed (initial fetch or a `state_changed` ping).
    StateChanged(AuraStateView),
    /// ① A segment's raw, evolving streaming output. Update the live UI fast.
    StreamFragment { window_id: u64, segment_id: u64, text: String },
    /// ② A segment's batch pass (per-segment batch, at EOS).
    BatchSegment { window_id: u64, segment_id: u64, text: String },
    /// ③ The whole-window batch re-run (per WindowEdge) — authoritative raw_text.
    BatchWindow { window_id: u64, text: String },
    /// ④ Stage2 jointly calibrated the current window's segments so far (per Batch) — the
    /// calibrated text, same window, replaces the previous calibration.
    SegmentCalibration { window_id: u64, calibrated: String },
    /// ⑤ A window settled (WindowEdge's authoritative final) — appended to /
    /// updated in [`AuraAgent::windows`].
    WindowCalibration(WindowView),
    /// A correction for a window was accepted (the list entry is marked `corrected`).
    WindowCorrected(u64),
}

// ── Per-window / per-segment recognition state ────────────────────────────
//
// `get_window_preview` 需要知道窗口里每个 Segment 当前是什么:
// 是还在流式(StreamFragment)还是已出 batch(BatchSegment)、整窗是否已关
// 闭(BatchWindow/WindowCalibration)、Stage2 校准到了哪一层。这里用
// `windows_state` 在事件流入时折叠出这个视图。

/// 一个 Segment 的识别状态。
#[derive(Debug, Clone, Default)]
struct SegmentState {
    /// 最新的 StreamFragment 文本(正在识别)。
    stream: String,
    /// BatchSegment 结果(该 Segment EOS 出 batch 后才有)。
    batch: Option<String>,
}

/// 一个窗口的识别状态,由数据面事件折叠而来。
#[derive(Debug, Clone, Default)]
struct WindowState {
    /// 整窗已关闭(BatchWindow / WindowCalibration 到达)。
    closed: bool,
    /// 整窗 batch 重跑文本(BatchWindow,权威 raw)。
    batch_window: Option<String>,
    /// 定稿校准文本(WindowCalibration)。
    calibrated: Option<String>,
    /// Stage2 对当前窗口所有已到 Segment 的临时联合校准(SegmentCalibration)。
    segment_calibration: Option<String>,
    /// 各 Segment,按首见顺序。
    segments: Vec<(u64, SegmentState)>,
}

impl WindowState {
    /// 逐段拼接:每段有 BatchSegment 用 BatchSegment,否则用 StreamFragment。
    fn concat_segments(&self) -> String {
        let mut out = String::new();
        for (_, seg) in &self.segments {
            let text = seg.batch.clone().unwrap_or_else(|| seg.stream.clone());
            out.push_str(&text);
        }
        out
    }

    /// 基本预览(plain):窗口已关闭 → BatchWindow;未关闭 → 逐段拼接。
    fn plain_preview(&self) -> String {
        if self.closed {
            self.batch_window.clone().unwrap_or_default()
        } else {
            self.concat_segments()
        }
    }

    /// 校准优先预览(calc):窗口已关闭 → WindowCalibration 优先于 BatchWindow;
    /// 未关闭 → SegmentCalibration 优先于逐段拼接,识别中段用 StreamFragment。
    fn calc_preview(&self) -> String {
        if self.closed {
            return self.calibrated.clone()
                .or_else(|| self.batch_window.clone())
                .unwrap_or_default();
        }
        match &self.segment_calibration {
            Some(sc) if !sc.is_empty() => sc.clone(),
            _ => self.concat_segments(),
        }
    }
}

/// Managed-state client facade. Cheap to clone (shares the driver + state).
#[derive(Clone)]
pub struct AuraAgent {
    inner: Arc<AgentInner>,
}

// Identity = the daemon origin — lets iced-style subscription ids dedup by address without
// hashing any mutable state. (Manual Debug: AgentInner holds a tokio Runtime, which isn't.)
impl std::fmt::Debug for AuraAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AuraAgent").field(&self.inner.client.base()).finish()
    }
}

impl std::hash::Hash for AuraAgent {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.inner.client.base().hash(state);
    }
}

struct AgentInner {
    client: AuraClient,
    state: RwLock<Option<AuraStateView>>,
    windows: RwLock<Vec<WindowView>>,
    /// 每窗口识别状态(折叠数据面事件)—— `get_window_preview` 的源。
    windows_state: RwLock<HashMap<u64, WindowState>>,
    live: RwLock<Option<(u64, String)>>,
    conn: AtomicU8,
    tx: tokio::sync::broadcast::Sender<AgentEvent>,
    /// Shared receiver for [`AuraAgent::poll_events`] (sync clients — no tokio runtime needed).
    poll: Mutex<tokio::sync::broadcast::Receiver<AgentEvent>>,
    /// 三个 driver 任务句柄(health / segments / control),start 时填,drop 时 cancel。
    driver_tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl AuraAgent {
    /// 构造一个未启动的 agent。`base` 可以是裸 `host:port` 或完整 origin。
    /// **不起 runtime、driver 线程**—— 这些由调用方通过 [`AuraAgent::start`] 提供。
    pub fn new(base: impl Into<String>) -> Result<Self> {
        let base = base.into();
        let base = if base.starts_with("http://") || base.starts_with("https://") {
            base
        } else {
            format!("http://{base}")
        };
        let client = AuraClient::new(&base)?;
        let (tx, rx) = tokio::sync::broadcast::channel(EVENT_CAP);
        let inner = Arc::new(AgentInner {
            client,
            state: RwLock::new(None),
            windows: RwLock::new(Vec::new()),
            windows_state: RwLock::new(HashMap::new()),
            live: RwLock::new(None),
            conn: AtomicU8::new(AuraConn::Connecting as u8),
            tx,
            poll: Mutex::new(rx),
            driver_tasks: Mutex::new(Vec::new()),
        });
        Ok(Self { inner })
    }

    /// 在调用方提供的 runtime handle 上启动 driver 三个任务(health / segments /
    /// control plane)。返回的 future 在 driver 三个任务**全部**结束时 resolve —— agent
    /// 析构(driver_tasks join handle drop → task abort)也会让它返回。
    ///
    /// 调用方负责保证 `handle` 来自的 runtime 持续存活。
    pub async fn start(self: &Arc<Self>, handle: &Handle) {
        let mut guard = self.inner.driver_tasks.lock().unwrap();
        // 已启动:no-op,避免覆盖。
        if !guard.is_empty() { return; }
        let inner = Arc::clone(&self.inner);
        let h1 = handle.spawn(driver_health(inner.clone()));
        let h2 = handle.spawn(driver_segments(inner.clone()));
        let h3 = handle.spawn(driver_control(inner));
        *guard = vec![h1, h2, h3];
    }

    /// Current control-plane snapshot (None until the first fetch completes).
    pub fn state(&self) -> Option<AuraStateView> {
        self.inner.state.read().unwrap().clone()
    }

    /// Accumulated settled windows (ascending window_id). Clone — clients own a copy.
    pub fn windows(&self) -> Vec<WindowView> {
        self.inner.windows.read().unwrap().clone()
    }

    /// The live window's text `(window_id, text)`, if any.
    pub fn live(&self) -> Option<(u64, String)> {
        self.inner.live.read().unwrap().clone()
    }

    /// 一个窗口的**基本预览**:窗口已关闭 → 显示该窗口的 `BatchWindow` 结果;
    /// 未关闭 → 逐段拼接,每段优先 `BatchSegment`,仍在识别则用 `StreamFragment`。
    ///
    /// 未知窗口返回 `None`。
    pub fn get_window_preview(&self, window_id: u64) -> Option<String> {
        let map = self.inner.windows_state.read().unwrap();
        map.get(&window_id).map(|w| w.plain_preview())
    }

    /// 一个窗口的**校准优先预览**(`#asr/calc` 集成):
    /// - 已关闭:`WindowCalibration` 优先于 `BatchWindow`;
    /// - 未关闭:`SegmentCalibration` 优先于逐段拼接,识别中段用 `StreamFragment`。
    ///
    /// 未知窗口返回 `None`。
    pub fn get_window_calc_preview(&self, window_id: u64) -> Option<String> {
        let map = self.inner.windows_state.read().unwrap();
        map.get(&window_id).map(|w| w.calc_preview())
    }

    /// Current connectivity (best-effort, last known).
    pub fn conn(&self) -> AuraConn {
        AuraConn::from_u8(self.inner.conn.load(Ordering::Relaxed))
    }

    /// Every agent event (live text, finals, snapshots, connectivity) — push semantics. The
    /// stream never ends (reconnects internally). For poll-style clients, `state`/`utterances`/
    /// `live`/`conn` are always readable synchronously.
    pub fn events(&self) -> impl Stream<Item = AgentEvent> + '_ {
        async_stream::stream! {
            let mut rx = self.inner.tx.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) => yield ev,
                    // Slow consumer: the queue dropped events we haven't read. State is kept
                    // authoritative in the RwLocks, so skipping stale events is safe.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    /// Drain pending events, non-blocking (sync clients — no tokio runtime needed). Call this in
    /// your own loop; state is additionally always readable via `state`/`utterances`/`live`/`conn`.
    pub fn poll_events(&self) -> Vec<AgentEvent> {
        let mut rx = self.inner.poll.lock().unwrap();
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(ev) => out.push(ev),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
        out
    }

    /// `POST /api/control/scout {enabled}` — fire-and-forget. 调用方在外部 runtime 上
    /// `spawn(agent.set_connected(...))` 即可。
    pub async fn set_connected(&self, enabled: bool) -> Result<()> {
        self.inner.client.set_connected(enabled).await.map(|_| ())
    }

    /// `POST /api/correct` — record a user correction for a window (feeds Stage2).
    /// 异步,调用方负责 runtime 上下文。
    pub async fn correct(&self, window_id: u64, raw: &str, corrected: &str) -> Result<()> {
        self.inner.client.correct(window_id, raw, corrected).await
    }

    /// `GET /api/audio/{window_id}` — the window's WAV bytes (for playback). 异步。
    pub async fn audio(&self, window_id: u64) -> Result<Vec<u8>> {
        self.inner.client.audio(window_id).await
    }
}

impl AuraAgent {
    /// 测试用构造:不起 driver,保留 client + 状态通道(已迁入)。
    #[cfg(test)]
    fn for_test() -> Arc<Self> {
        let inner = Arc::new(AgentInner {
            client: AuraClient::new("http://127.0.0.1:1").unwrap(),
            state: RwLock::new(None),
            windows: RwLock::new(Vec::new()),
            windows_state: RwLock::new(HashMap::new()),
            live: RwLock::new(None),
            conn: AtomicU8::new(AuraConn::Connecting as u8),
            tx: tokio::sync::broadcast::channel(EVENT_CAP).0,
            poll: Mutex::new(tokio::sync::broadcast::channel(EVENT_CAP).1),
            driver_tasks: Mutex::new(Vec::new()),
        });
        Arc::new(Self { inner })
    }
}

/// Health 探针任务:周期性 `GET /health`,状态变化时更新 conn + 触发重连时
/// 主动拉一次快照对齐。每个 driver 任务都是独立 future,由 `start()` 在外部
/// runtime 上 `spawn` —— 引擎 drop 时 JoinHandle drop → task 自动 abort。
async fn driver_health(inner: Arc<AgentInner>) {
    let client = inner.client.clone();
    loop {
        let ok = client.health().await.unwrap_or_else(|e| {
            tracing::debug!(error = %e, "health probe failed");
            false
        });
        let c = AuraConn::from_ok(ok);
        let prev = AuraConn::from_u8(inner.conn.load(Ordering::Relaxed));
        if c != prev {
            // set_conn 在断开时清空客户端状态;这里补上重连的对齐:
            // Disconnected → Connected 时重新请求一次快照,立刻回到新状态。
            set_conn(&inner, c);
            if prev == AuraConn::Disconnected && c == AuraConn::Connected {
                match client.state().await {
                    Ok(snap) => set_state(&inner, snap),
                    Err(e) => tracing::warn!(error = %e, "reconnect snapshot fetch failed"),
                }
            }
        }
        tokio::time::sleep(HEALTH_PROBE).await;
    }
}

/// Data plane:订阅 AsrSegment 流,把每个事件折叠进共享状态 + 广播。
async fn driver_segments(inner: Arc<AgentInner>) {
    let mut segs = Box::pin(inner.client.subscribe_segments());
    while let Some(seg) = segs.next().await {
        set_conn(&inner, AuraConn::Connected); // any segment ⇒ stream is live
        apply_segment(&inner, seg);
    }
}

/// Control plane:首次拉取快照,再订阅 state_changed ping 触发再拉取。
async fn driver_control(inner: Arc<AgentInner>) {
    // Initial snapshot before anything else (clients want state ASAP).
    match inner.client.state().await {
        Ok(snap) => set_state(&inner, snap),
        Err(e) => tracing::warn!(error = %e, "initial state fetch failed; retrying on pings"),
    }
    let mut s = Box::pin(inner.client.subscribe(STATE_FREQ_MS));
    while s.next().await.is_some() {
        match inner.client.state().await {
            Ok(snap) => set_state(&inner, snap),
            Err(e) => tracing::warn!(error = %e, "state re-fetch failed"),
        }
    }
}

fn set_conn(inner: &AgentInner, c: AuraConn) {
    let old = AuraConn::from_u8(inner.conn.load(Ordering::Relaxed));
    if old != c {
        inner.conn.store(c as u8, Ordering::Relaxed);
        // 断开:清空客户端状态 —— 重连后从干净状态重新请求、对齐。不这么做,
        // 旧的 windows/live/快照会一直挂着,断开期间看到的还是断线前的语音。
        if c == AuraConn::Disconnected {
            *inner.state.write().unwrap() = None;
            inner.windows.write().unwrap().clear();
            inner.windows_state.write().unwrap().clear();
            *inner.live.write().unwrap() = None;
            tracing::info!("aura disconnected — client state cleared");
        }
        let _ = inner.tx.send(AgentEvent::ConnChanged(c));
    }
}

fn set_state(inner: &AgentInner, snap: AuraStateView) {
    *inner.state.write().unwrap() = Some(snap.clone());
    let _ = inner.tx.send(AgentEvent::StateChanged(snap));
}

/// 取(或建)一个窗口的识别状态,就地更新。
fn with_window_state(
    ws: &RwLock<HashMap<u64, WindowState>>,
    window_id: u64,
    f: impl FnOnce(&mut WindowState),
) {
    let mut map = ws.write().unwrap();
    let win = map.entry(window_id).or_default();
    f(win);
}

/// 取(或建)窗口里的一个 Segment 状态,就地更新(按 segment_id upsert)。
fn with_segment_state(
    ws: &RwLock<HashMap<u64, WindowState>>,
    window_id: u64,
    segment_id: u64,
    f: impl FnOnce(&mut SegmentState),
) {
    with_window_state(ws, window_id, |win| {
        match win.segments.iter_mut().find(|(id, _)| *id == segment_id) {
            Some((_, seg)) => f(seg),
            None => {
                win.segments.push((segment_id, SegmentState::default()));
                f(&mut win.segments.last_mut().unwrap().1);
            }
        }
    });
}

/// Fold one data-plane segment into the shared window list + live text. Pure enough to unit
/// test (only touches the RwLocks + event queue).
fn apply_segment(inner: &AgentInner, seg: AsrSegment) {
    match seg {
        AsrSegment::StreamFragment { window_id, segment_id, text, .. } => {
            *inner.live.write().unwrap() = Some((window_id, text.clone()));
            with_segment_state(&inner.windows_state, window_id, segment_id, |s| s.stream = text.clone());
            let _ = inner.tx.send(AgentEvent::StreamFragment { window_id, segment_id, text });
        }
        AsrSegment::BatchSegment { window_id, segment_id, text } => {
            with_segment_state(&inner.windows_state, window_id, segment_id, |s| s.batch = Some(text.clone()));
            let _ = inner.tx.send(AgentEvent::BatchSegment { window_id, segment_id, text });
        }
        AsrSegment::BatchWindow { window_id, text } => {
            with_window_state(&inner.windows_state, window_id, |w| {
                w.closed = true;
                w.batch_window = Some(text.clone());
            });
            let _ = inner.tx.send(AgentEvent::BatchWindow { window_id, text });
        }
        AsrSegment::SegmentCalibration { window_id, calibrated } => {
            *inner.live.write().unwrap() = Some((window_id, calibrated.clone()));
            with_window_state(&inner.windows_state, window_id, |w| {
                w.segment_calibration = Some(calibrated.clone());
            });
            let _ = inner.tx.send(AgentEvent::SegmentCalibration { window_id, calibrated });
        }
        AsrSegment::WindowCalibration { window_id, calibrated } => {
            *inner.live.write().unwrap() = None;
            with_window_state(&inner.windows_state, window_id, |w| {
                w.closed = true;
                w.calibrated = Some(calibrated.clone());
            });
            let mut wins = inner.windows.write().unwrap();
            // Upsert by window_id (defensive — windows settle in ascending order, but a
            // reconnect could replay an older one). Preserve the corrected flag.
            if let Some(existing) = wins.iter_mut().find(|w| w.window_id == window_id) {
                existing.calibrated = calibrated;
            } else {
                wins.push(WindowView { window_id, calibrated, corrected: false });
                wins.sort_by_key(|w| w.window_id);
            }
            let view = wins.iter().find(|w| w.window_id == window_id).cloned();
            drop(wins);
            if let Some(view) = view {
                let _ = inner.tx.send(AgentEvent::WindowCalibration(view));
            }
        }
        AsrSegment::Correction { window_id, .. } => {
            if let Some(w) =
                inner.windows.write().unwrap().iter_mut().find(|w| w.window_id == window_id)
            {
                w.corrected = true;
            }
            let _ = inner.tx.send(AgentEvent::WindowCorrected(window_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner() -> Arc<AgentInner> {
        AuraAgent::for_test().inner.clone()
    }

    #[test]
    fn folds_window_calibration_into_windows_and_clears_live() {
        let i = inner();
        apply_segment(&i, AsrSegment::SegmentCalibration { window_id: 1, calibrated: "蛇声".into() });
        assert_eq!(i.live.read().unwrap().as_ref().map(|(_, t)| t.as_str()), Some("蛇声"));
        apply_segment(&i, AsrSegment::WindowCalibration { window_id: 1, calibrated: "蛇身".into() });
        assert!(i.live.read().unwrap().is_none(), "WindowCalibration clears the live text");
        let wins = i.windows.read().unwrap();
        assert_eq!(wins.len(), 1);
        assert_eq!(wins[0].window_id, 1);
        assert_eq!(wins[0].calibrated, "蛇身");
        assert!(!wins[0].corrected);
    }

    #[test]
    fn stream_fragment_keys_live_by_window() {
        let i = inner();
        apply_segment(
            &i,
            AsrSegment::StreamFragment { window_id: 7, segment_id: 3, text: "你好".into(), at_s: 1.0 },
        );
        assert_eq!(i.live.read().unwrap().as_ref().map(|(w, _)| *w), Some(7));
    }

    #[test]
    fn batch_segment_and_window_forward_without_live() {
        let i = inner();
        apply_segment(&i, AsrSegment::BatchSegment { window_id: 7, segment_id: 3, text: "你好".into() });
        assert!(i.live.read().unwrap().is_none(), "BatchSegment does not touch live");
        apply_segment(&i, AsrSegment::BatchWindow { window_id: 7, text: "你好".into() });
        assert!(i.live.read().unwrap().is_none(), "BatchWindow does not touch live");
    }

    #[test]
    fn correction_marks_window() {
        let i = inner();
        apply_segment(&i, AsrSegment::WindowCalibration { window_id: 2, calibrated: "蛇声".into() });
        apply_segment(
            &i,
            AsrSegment::Correction { window_id: 2, raw: "蛇声".into(), corrected: "蛇身".into() },
        );
        assert!(i.windows.read().unwrap()[0].corrected);
    }

    #[test]
    fn windows_are_sorted_by_id() {
        let i = inner();
        for window_id in [3u64, 1, 2] {
            apply_segment(&i, AsrSegment::WindowCalibration { window_id, calibrated: "x".into() });
        }
        let ids: Vec<u64> = i.windows.read().unwrap().iter().map(|w| w.window_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    // ── get_window_preview ──────────────────────────────────────────────

    /// 喂一段流式 + 对应 batch(模拟一个已出 batch 的 Segment)。
    fn stream_then_batch(i: &Arc<AgentInner>, wid: u64, sid: u64, text: &str) {
        apply_segment(
            &i,
            AsrSegment::StreamFragment { window_id: wid, segment_id: sid, text: text.into(), at_s: 0.0 },
        );
        apply_segment(&i, AsrSegment::BatchSegment { window_id: wid, segment_id: sid, text: text.into() });
    }

    fn preview(i: &Arc<AgentInner>, wid: u64) -> (Option<String>, Option<String>) {
        let a = AuraAgent { inner: Arc::clone(i) };
        (a.get_window_preview(wid), a.get_window_calc_preview(wid))
    }

    #[test]
    fn open_window_preview_uses_stream_fragment_while_recognizing() {
        let i = inner();
        apply_segment(
            &i,
            AsrSegment::StreamFragment { window_id: 5, segment_id: 1, text: "你好".into(), at_s: 0.0 },
        );
        // 未关闭,只有 StreamFragment → 拼接取它。
        let (plain, calc) = preview(&i, 5);
        assert_eq!(plain.as_deref(), Some("你好"));
        assert_eq!(calc.as_deref(), Some("你好"), "无 SegmentCalibration 时 calc 同样取流式");
    }

    #[test]
    fn open_window_preview_prefers_batch_segment_over_stream() {
        let i = inner();
        apply_segment(
            &i,
            AsrSegment::StreamFragment { window_id: 5, segment_id: 1, text: "你好".into(), at_s: 0.0 },
        );
        apply_segment(&i, AsrSegment::BatchSegment { window_id: 5, segment_id: 1, text: "你好".into() });
        // 已出 batch → 用 BatchSegment。
        assert_eq!(preview(&i, 5).0.as_deref(), Some("你好"));
    }

    #[test]
    fn open_window_preview_concats_segments_in_order() {
        let i = inner();
        stream_then_batch(&i, 9, 1, "第一段");
        // 第二段还在识别。
        apply_segment(
            &i,
            AsrSegment::StreamFragment { window_id: 9, segment_id: 2, text: "第二段".into(), at_s: 0.0 },
        );
        // 未关闭 → 逐段拼接:第一段取 BatchSegment,第二段取 StreamFragment。
        assert_eq!(preview(&i, 9).0.as_deref(), Some("第一段第二段"));
    }

    #[test]
    fn closed_window_preview_uses_batch_window() {
        let i = inner();
        stream_then_batch(&i, 4, 1, "内容");
        apply_segment(&i, AsrSegment::BatchWindow { window_id: 4, text: "整窗batch".into() });
        // 已关闭 → plain 显示 BatchWindow。
        assert_eq!(preview(&i, 4).0.as_deref(), Some("整窗batch"));
        // calc:无 WindowCalibration 时回退 BatchWindow。
        assert_eq!(preview(&i, 4).1.as_deref(), Some("整窗batch"));
    }

    #[test]
    fn calc_closed_prefers_window_calibration_over_batch_window() {
        let i = inner();
        stream_then_batch(&i, 4, 1, "内容");
        apply_segment(&i, AsrSegment::BatchWindow { window_id: 4, text: "整窗batch".into() });
        apply_segment(&i, AsrSegment::WindowCalibration { window_id: 4, calibrated: "定稿".into() });
        // plain 仍显示 BatchWindow(未集成校准)。
        assert_eq!(preview(&i, 4).0.as_deref(), Some("整窗batch"));
        // calc 优先 WindowCalibration。
        assert_eq!(preview(&i, 4).1.as_deref(), Some("定稿"));
    }

    #[test]
    fn calc_open_prefers_segment_calibration_over_concat() {
        let i = inner();
        stream_then_batch(&i, 7, 1, "第一段");
        apply_segment(&i, AsrSegment::StreamFragment { window_id: 7, segment_id: 2, text: "第二段".into(), at_s: 0.0 });
        assert_eq!(preview(&i, 7).1.as_deref(), Some("第一段第二段"), "无校准 → 逐段拼接");
        // Stage2 联合校准到达 → calc 用它。
        apply_segment(&i, AsrSegment::SegmentCalibration { window_id: 7, calibrated: "联合整流".into() });
        assert_eq!(preview(&i, 7).1.as_deref(), Some("联合整流"));
        // plain 不受影响(仍拼接)。
        assert_eq!(preview(&i, 7).0.as_deref(), Some("第一段第二段"));
    }

    #[test]
    fn unknown_window_preview_is_none() {
        let i = inner();
        assert_eq!(preview(&i, 42).0, None);
        assert_eq!(preview(&i, 42).1, None);
    }

    #[test]
    fn disconnect_clears_client_state() {
        // 断开连接时清空客户端状态,重连后从干净状态重新请求对齐。
        let i = inner();
        // 造出三样状态:定稿窗口 + 新窗口在流(live 有值)+ 控制面快照。
        apply_segment(&i, AsrSegment::WindowCalibration { window_id: 1, calibrated: "定稿".into() });
        apply_segment(
            &i,
            AsrSegment::StreamFragment { window_id: 2, segment_id: 1, text: "新窗口".into(), at_s: 0.0 },
        );
        let snap: AuraStateView = serde_json::from_str(
            r#"{"connected":true,"stage3_on":false,"config":{"asr_backend":"","asr_kind":"","asr_provider":"","llm_kind":"","model":"","vad":{"threshold":0.5,"min_silence":0.3,"merge_gap":1.0}},"hotwords":[],"corrections":[]}"#,
        ).unwrap();
        set_state(&i, snap);

        assert!(i.state.read().unwrap().is_some());
        assert_eq!(i.windows.read().unwrap().len(), 1);
        assert_eq!(i.windows_state.read().unwrap().len(), 2);
        assert!(i.live.read().unwrap().is_some());

        // 断开 → 全部清空。
        set_conn(&i, AuraConn::Disconnected);
        assert!(i.state.read().unwrap().is_none(), "snapshot cleared");
        assert!(i.windows.read().unwrap().is_empty(), "settled windows cleared");
        assert!(i.windows_state.read().unwrap().is_empty(), "window state cleared");
        assert!(i.live.read().unwrap().is_none(), "live cleared");
    }
}
