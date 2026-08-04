//! agent.rs — `AuraAgent`: the managed-state facade over [`AuraClient`]. A client (swift-ime,
//! geek-familiar, …) imports this crate and calls [`AuraAgent::connect`] once — the agent spawns
//! its own background driver (tokio runtime on a std thread) that keeps EVERYTHING fresh:
//!
//! - **Control plane**: the latest [`AuraStateView`] snapshot (re-fetched on each `state_changed`
//!   ping), readable synchronously via [`AuraAgent::state`].
//! - **Data plane**: the live recognition text ([`AuraAgent::live`]) and the accumulated
//!   utterance list ([`AuraAgent::utterances`], with corrected flags), built from the
//!   `AsrSegment` stream.
//! - **Connectivity**: a periodic `/health` probe ([`AuraAgent::conn`]).
//!
//! Every change also surfaces as an [`AgentEvent`] on [`AuraAgent::events`] for clients that want
//! push semantics (e.g. a UI that updates on each Interim/Final). Commands (`set_connected`,
//! `correct`, `audio`) are one-liners. No client-side connection or state-management code needed.
//!
//! ```ignore
//! use audio_aura_agent::agent::AuraAgent;
//! let agent = AuraAgent::connect("http://127.0.0.1:9091")?;
//! let utts = agent.utterances();        // sync read, never blocks
//! agent.correct(3, "蛇声", "蛇身");      // fire-and-forget command
//! ```

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use futures::{Stream, StreamExt};
use tokio::runtime::Runtime;

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

/// One settled utterance, as the agent maintains it from the data plane (Final + Correction
/// segments). The snapshot carries no utterances — this list is the client's transcript source.
#[derive(Debug, Clone)]
pub struct UtteranceView {
    pub seq: u64,
    pub raw_text: String,
    pub streaming_text: String,
    pub calibrated: String,
    pub intent: String,
    pub reply: String,
    pub route_ms: f64,
    /// True once a `Correction` segment for this seq arrived (the pair also enters Stage2).
    pub corrected: bool,
}

/// Everything the agent surfaces to the client — read via [`AuraAgent::events`]. Three
/// recognition events, one per stage, mirroring the daemon's `AsrSegment` variants:
/// ① [`Interim`](AgentEvent::Interim) — a new Stage1 streaming fragment (raw partial); ②
/// [`CalibratedInterim`](AgentEvent::CalibratedInterim) — Stage2 finished correcting a batch
/// (small Batch or big MergeBatch — both land here while the utterance is still in progress);
/// ③ [`TurnFinal`](AgentEvent::TurnFinal) — the merged paragraph settled (authoritative).
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Connectivity changed (reconnecting / connected / lost).
    ConnChanged(AuraConn),
    /// The control-plane snapshot refreshed (initial fetch or a `state_changed` ping).
    StateChanged(AuraStateView),
    /// ① A new Stage1 streaming fragment — the raw, evolving partial. Update the live UI fast.
    /// `seq` is the in-progress utterance's prospective seq.
    Interim { seq: u64, partial: String },
    /// ② Stage2 finished correcting a batch (small Batch per merged fragment, or the big
    /// MergeBatch's provisional) — the calibrated text so far, same seq, updates in place.
    CalibratedInterim { seq: u64, calibrated: String },
    /// ③ A settled utterance arrived (the big MergeBatch's authoritative Final) — appended to /
    /// updated in [`AuraAgent::utterances`].
    TurnFinal(UtteranceView),
    /// A correction for seq was accepted (the list entry is marked `corrected`).
    TurnCorrected(u64),
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
    rt: Runtime,
    state: RwLock<Option<AuraStateView>>,
    utterances: RwLock<Vec<UtteranceView>>,
    live: RwLock<Option<(u64, String)>>,
    conn: AtomicU8,
    tx: tokio::sync::broadcast::Sender<AgentEvent>,
    /// Shared receiver for [`AuraAgent::poll_events`] (sync clients — no tokio runtime needed).
    poll: Mutex<tokio::sync::broadcast::Receiver<AgentEvent>>,
    running: AtomicBool,
}

impl AuraAgent {
    /// Connect + start the background driver. Returns immediately (non-blocking); the first
    /// snapshot arrives on `events()` (and `state()` once fetched). `base` may be a bare
    /// `host:port` or a full origin.
    pub fn connect(base: impl Into<String>) -> Result<Self> {
        let base = base.into();
        let base = if base.starts_with("http://") || base.starts_with("https://") {
            base
        } else {
            format!("http://{base}")
        };
        let client = AuraClient::new(&base)?;
        let rt = Runtime::new()?;
        let (tx, rx) = tokio::sync::broadcast::channel(EVENT_CAP);
        let inner = Arc::new(AgentInner {
            client,
            rt,
            state: RwLock::new(None),
            utterances: RwLock::new(Vec::new()),
            live: RwLock::new(None),
            conn: AtomicU8::new(AuraConn::Connecting as u8),
            tx,
            poll: Mutex::new(rx),
            running: AtomicBool::new(true),
        });
        let driver = Arc::clone(&inner);
        thread::Builder::new()
            .name("aura-agent-driver".into())
            .spawn(move || {
                let rt = &driver.rt;
                rt.block_on(driver_loop(&driver));
            })
            .expect("spawn aura-agent driver thread");
        Ok(Self { inner })
    }

    /// Current control-plane snapshot (None until the first fetch completes).
    pub fn state(&self) -> Option<AuraStateView> {
        self.inner.state.read().unwrap().clone()
    }

    /// Accumulated settled utterances (ascending seq). Clone — clients own a copy.
    pub fn utterances(&self) -> Vec<UtteranceView> {
        self.inner.utterances.read().unwrap().clone()
    }

    /// The in-progress utterance's live text `(seq, text)`, if any.
    pub fn live(&self) -> Option<(u64, String)> {
        self.inner.live.read().unwrap().clone()
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

    /// `POST /api/control/scout {enabled}` — fire-and-forget (runs on the driver runtime).
    pub fn set_connected(&self, enabled: bool) {
        let c = self.inner.client.clone();
        self.inner.rt.spawn(async move {
            let _ = c.set_connected(enabled).await;
        });
    }

    /// `POST /api/correct` — record a user correction (feeds Stage2). Fire-and-forget.
    pub fn correct(&self, seq: u64, raw: &str, corrected: &str) {
        let c = self.inner.client.clone();
        let raw = raw.to_string();
        let corrected = corrected.to_string();
        self.inner.rt.spawn(async move {
            let _ = c.correct(seq, &raw, &corrected).await;
        });
    }

    /// `GET /api/audio/{seq}` — the utterance's WAV bytes (blocking; runs on the driver runtime).
    pub fn audio(&self, seq: u64) -> Result<Vec<u8>> {
        let c = self.inner.client.clone();
        self.inner.rt.block_on(c.audio(seq))
    }
}

impl Drop for AuraAgent {
    fn drop(&mut self) {
        self.inner.running.store(false, Ordering::Relaxed);
    }
}

/// The background driver: fetches the initial snapshot, then runs three loops concurrently —
/// control-plane snapshot sync, data-plane segments, and the health probe — writing shared state
/// and forwarding events. Runs until the agent is dropped (`running` flips false).
async fn driver_loop(inner: &Arc<AgentInner>) {
    // Initial snapshot before anything else (clients want state ASAP).
    match inner.client.state().await {
        Ok(snap) => set_state(inner, snap),
        Err(e) => tracing::warn!(error = %e, "initial state fetch failed; retrying on pings"),
    }
    let health = inner.client.clone();
    let hc_conn = Arc::clone(inner);
    let hc = tokio::spawn(async move {
        while hc_conn.running.load(Ordering::Relaxed) {
            let ok = health.health().await.unwrap_or(false);
            set_conn(&hc_conn, AuraConn::from_ok(ok));
            tokio::time::sleep(HEALTH_PROBE).await;
        }
    });

    // Data plane: live segments → shared state + events. The client is cloned into the task so
    // the borrow-free stream is 'static (the SDK's streams borrow their client).
    let seg_inner = Arc::clone(inner);
    let seg_client = inner.client.clone();
    let seg_task = tokio::spawn(async move {
        let mut segs = Box::pin(seg_client.subscribe_segments());
        while let Some(seg) = segs.next().await {
            set_conn(&seg_inner, AuraConn::Connected); // any segment ⇒ stream is live
            apply_segment(&seg_inner, seg);
        }
    });

    // Control plane: re-fetch the snapshot on each state_changed ping.
    let ctrl_inner = Arc::clone(inner);
    let ctrl_client = inner.client.clone();
    let ctrl = tokio::spawn(async move {
        let mut s = Box::pin(ctrl_client.subscribe(STATE_FREQ_MS));
        while s.next().await.is_some() {
            match ctrl_client.state().await {
                Ok(snap) => set_state(&ctrl_inner, snap),
                Err(e) => tracing::warn!(error = %e, "state re-fetch failed"),
            }
        }
    });

    // Wait until the agent is dropped, then let the loops wind down (the spawned tasks check
    // `running` / end on stream close; block_on returning frees the driver thread).
    while inner.running.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = hc.await;
    let _ = seg_task.await;
    let _ = ctrl.await;
}

fn set_conn(inner: &AgentInner, c: AuraConn) {
    let old = AuraConn::from_u8(inner.conn.load(Ordering::Relaxed));
    if old != c {
        inner.conn.store(c as u8, Ordering::Relaxed);
        let _ = inner.tx.send(AgentEvent::ConnChanged(c));
    }
}

fn set_state(inner: &AgentInner, snap: AuraStateView) {
    *inner.state.write().unwrap() = Some(snap.clone());
    let _ = inner.tx.send(AgentEvent::StateChanged(snap));
}

/// Fold one data-plane segment into the shared utterance list + live text. Pure enough to unit
/// test (only touches the RwLocks + event queue).
fn apply_segment(inner: &AgentInner, seg: AsrSegment) {
    match seg {
        AsrSegment::Interim { seq, partial, .. } => {
            *inner.live.write().unwrap() = Some((seq, partial.clone()));
            let _ = inner.tx.send(AgentEvent::Interim { seq, partial });
        }
        AsrSegment::CalibratedInterim { seq, calibrated } => {
            *inner.live.write().unwrap() = Some((seq, calibrated.clone()));
            let _ = inner.tx.send(AgentEvent::CalibratedInterim { seq, calibrated });
        }
        AsrSegment::Final { seq, raw_text, streaming_text, calibrated, intent, reply, route_ms } => {
            *inner.live.write().unwrap() = None;
            let mut utts = inner.utterances.write().unwrap();
            // Upsert by seq (defensive — Finals settle in ascending order, but a reconnect could
            // replay an older one).
            if let Some(existing) = utts.iter_mut().find(|u| u.seq == seq) {
                *existing = UtteranceView {
                    seq, raw_text, streaming_text, calibrated, intent, reply, route_ms,
                    corrected: existing.corrected,
                };
            } else {
                utts.push(UtteranceView {
                    seq, raw_text, streaming_text, calibrated, intent, reply, route_ms,
                    corrected: false,
                });
                utts.sort_by_key(|u| u.seq);
            }
            let view = utts.iter().find(|u| u.seq == seq).cloned();
            drop(utts);
            if let Some(view) = view {
                let _ = inner.tx.send(AgentEvent::TurnFinal(view));
            }
        }
        AsrSegment::Correction { seq, .. } => {
            if let Some(u) = inner.utterances.write().unwrap().iter_mut().find(|u| u.seq == seq) {
                u.corrected = true;
            }
            let _ = inner.tx.send(AgentEvent::TurnCorrected(seq));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner() -> Arc<AgentInner> {
        let client = AuraClient::new("http://127.0.0.1:1").unwrap(); // never connects in tests
        let (tx, rx) = tokio::sync::broadcast::channel(EVENT_CAP);
        Arc::new(AgentInner {
            client,
            rt: Runtime::new().unwrap(),
            state: RwLock::new(None),
            utterances: RwLock::new(Vec::new()),
            live: RwLock::new(None),
            conn: AtomicU8::new(AuraConn::Connecting as u8),
            tx,
            poll: Mutex::new(rx),
            running: AtomicBool::new(true),
        })
    }

    #[test]
    fn folds_final_into_utterances_and_clears_live() {
        let i = inner();
        apply_segment(&i, AsrSegment::CalibratedInterim { seq: 1, calibrated: "蛇声".into() });
        assert_eq!(i.live.read().unwrap().as_ref().map(|(_, t)| t.as_str()), Some("蛇声"));
        apply_segment(
            &i,
            AsrSegment::Final {
                seq: 1,
                raw_text: "蛇声".into(),
                streaming_text: "蛇声".into(),
                calibrated: "蛇身".into(),
                intent: "chat".into(),
                reply: String::new(),
                route_ms: 12.3,
            },
        );
        assert!(i.live.read().unwrap().is_none(), "Final clears the live text");
        let utts = i.utterances.read().unwrap();
        assert_eq!(utts.len(), 1);
        assert_eq!(utts[0].seq, 1);
        assert_eq!(utts[0].calibrated, "蛇身");
        assert!(!utts[0].corrected);
    }

    #[test]
    fn correction_marks_utterance() {
        let i = inner();
        apply_segment(
            &i,
            AsrSegment::Final {
                seq: 2,
                raw_text: "蛇声".into(),
                streaming_text: "蛇声".into(),
                calibrated: "蛇声".into(),
                intent: "chat".into(),
                reply: String::new(),
                route_ms: 0.0,
            },
        );
        apply_segment(&i, AsrSegment::Correction { seq: 2, raw: "蛇声".into(), corrected: "蛇身".into() });
        assert!(i.utterances.read().unwrap()[0].corrected);
    }

    #[test]
    fn finals_are_sorted_by_seq() {
        let i = inner();
        for seq in [3u64, 1, 2] {
            apply_segment(
                &i,
                AsrSegment::Final {
                    seq,
                    raw_text: "x".into(),
                    streaming_text: "x".into(),
                    calibrated: "x".into(),
                    intent: "chat".into(),
                    reply: String::new(),
                    route_ms: 0.0,
                },
            );
        }
        let seqs: Vec<u64> = i.utterances.read().unwrap().iter().map(|u| u.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }
}
