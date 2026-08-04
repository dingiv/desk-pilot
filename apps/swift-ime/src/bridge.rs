//! Cross-subsystem communication.
//!
//! - **aura data-plane client**: connects to aura-daemon (`GET /api/asr_stream`) via the
//!   `audio-aura-agent` SDK. Runs on a dedicated tokio runtime in a background std thread; on
//!   each settled utterance (`AsrSegment::Final`) it writes the calibrated text into the shared
//!   [`AsrBuffer`], which the `#asr` magic command reads (non-blocking, microseconds) on the IME
//!   key-event path. Resilient — the SDK reconnects on drop.
//! - **familiar TCP server**: listens on :9601, accepts familiar connections for snippet config
//!   push + status display (Phase 2 stub).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use audio_aura_agent::client::AuraClient;
use audio_aura_agent::AsrSegment;
use futures::StreamExt;
use ime_core::asr_buffer::AsrBuffer;

/// Default aura-daemon origin.
const DEFAULT_AURA: &str = "http://127.0.0.1:9091";
/// Connectivity probe interval (the segment stream alone can't tell "connected but silent"
/// from "reconnecting" during pauses — a periodic /health ping can).
const HEALTH_PROBE: Duration = Duration::from_secs(3);

// ── Aura connectivity status ───────────────────────────────────────────

/// Aura-daemon connectivity, read by the TUI for display. Stored as a `u8` in an
/// [`AtomicU8`] so the background client (writer) and the render loop (reader) never contend.
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
        match v { 1 => AuraConn::Connected, 2 => AuraConn::Disconnected, _ => AuraConn::Connecting }
    }
}

/// Handle to the aura client's connectivity status. Cheap to clone; `.get()` is non-blocking.
#[derive(Clone)]
pub struct AuraConnHandle {
    status: Arc<AtomicU8>,
}

impl AuraConnHandle {
    /// Current connectivity (best-effort, last known).
    pub fn get(&self) -> AuraConn {
        AuraConn::from_u8(self.status.load(Ordering::Relaxed))
    }
}

// ── Public API ─────────────────────────────────────────────────────────

/// Spawn the aura data-plane client on a background thread. Returns a connectivity handle the
/// frontend can poll for display. Drives `subscribe_segments` on its own current-thread tokio
/// runtime; on each `Final` it writes the calibrated text into `buffer`. The IME main thread
/// never blocks — `AsrBuffer` is lock-guarded and only held microseconds per op.
///
/// `aura_addr` may be a bare `host:port` (http:// is prepended) or a full origin URL.
pub fn spawn_aura_client(buffer: Arc<AsrBuffer>, aura_addr: Option<&str>) -> AuraConnHandle {
    let base = normalize_origin(aura_addr.unwrap_or(DEFAULT_AURA));
    let status = Arc::new(AtomicU8::new(AuraConn::Connecting as u8));
    tracing::info!(addr = %base, "aura data-plane client starting");

    let status_for_thread = Arc::clone(&status);
    thread::Builder::new()
        .name("ime-aura-client".into())
        .spawn(move || run_aura_client(&base, buffer, status_for_thread))
        .expect("spawn aura client thread");

    AuraConnHandle { status }
}

fn run_aura_client(base: &str, buffer: Arc<AsrBuffer>, status: Arc<AtomicU8>) {
    // A single background thread drives the async SDK stream. current-thread runtime keeps the
    // footprint minimal (no worker pool).
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "failed to build tokio runtime for aura client");
            status.store(AuraConn::Disconnected as u8, Ordering::Relaxed);
            return;
        }
    };
    rt.block_on(async move {
        let client = match AuraClient::new(base) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "aura client init failed");
                status.store(AuraConn::Disconnected as u8, Ordering::Relaxed);
                return;
            }
        };

        // Periodic /health probe — sets Connected/Disconnected. Covers silent periods (segments
        // alone can't distinguish "connected, no speech" from "reconnecting").
        let hc_client = client.clone();
        let hc_status = Arc::clone(&status);
        tokio::spawn(async move {
            loop {
                let ok = hc_client.health().await.unwrap_or(false);
                hc_status.store(AuraConn::from_ok(ok) as u8, Ordering::Relaxed);
                tokio::time::sleep(HEALTH_PROBE).await;
            }
        });

        let segments = client.subscribe_segments();
        tokio::pin!(segments); // subscribe_segments is !Unpin — pin before .next()
        while let Some(seg) = segments.next().await {
            // A segment arriving ⇒ the stream is live.
            status.store(AuraConn::Connected as u8, Ordering::Relaxed);
            // Feed the voice session: streaming → live, settled → final (becomes candidate #1).
            match seg {
                AsrSegment::Interim { partial, .. } => {
                    if !partial.is_empty() {
                        buffer.set_live(&partial);
                        tracing::debug!(text = %partial, "asr live (interim)");
                    }
                }
                AsrSegment::CalibratedInterim { calibrated, .. } => {
                    if !calibrated.is_empty() {
                        buffer.set_live(&calibrated);
                        tracing::debug!(text = %calibrated, "asr live (calibrated)");
                    }
                }
                AsrSegment::Final { calibrated, .. } => {
                    if !calibrated.is_empty() {
                        buffer.push_final(&calibrated);
                        tracing::info!(text = %calibrated, "asr final → candidate #1");
                    }
                }
                AsrSegment::Correction { .. } => {} // not relevant to the voice buffer
            }
        }
        // subscribe_segments is infinite (it reconnects internally) — reaching here means the
        // client was dropped / shut down.
    });
}

/// Turn a bare `host:port` into an `http://host:port` origin (leave full URLs alone).
fn normalize_origin(addr: &str) -> String {
    let addr = addr.trim();
    if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    }
}

// ── Familiar TCP server (Phase 2 stub) ────────────────────────────────

pub fn spawn_familiar_server() {
    tracing::info!("familiar TCP server :9601 — stub (Phase 2)");
}

#[cfg(test)]
mod tests {
    use super::normalize_origin;

    #[test]
    fn bare_hostport_gets_http_scheme() {
        assert_eq!(normalize_origin("127.0.0.1:9091"), "http://127.0.0.1:9091");
    }

    #[test]
    fn full_url_unchanged() {
        assert_eq!(normalize_origin("http://1.2.3.4:9091"), "http://1.2.3.4:9091");
        assert_eq!(normalize_origin("https://x.io"), "https://x.io");
    }
}
