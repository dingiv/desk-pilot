//! Cross-subsystem communication.
//!
//! - **aura data-plane client**: the `audio-aura-agent` SDK's [`AuraAgent`] owns the
//!   connection — it exposes pure-async APIs (`new` + `start(handle)`); the engine's
//!   `IoThread` (single tokio runtime on `ime-io` thread) is the runtime. A periodic
//!   drain task lives on the same runtime and writes calibrated text into the shared
//!   [`AsrBuffer`] — the `#asr` magic command reads it on the IME key-event path.
//!   **No tokio runtime, no drain thread on the bridge side** — only a connection handle
//!   that the frontends poll for status display.
//! - **familiar TCP server**: listens on :9601, accepts familiar connections for snippet config
//!   push + status display (Phase 2 stub).

use std::sync::Arc;
use std::time::Duration;

use audio_aura_agent::agent::{AgentEvent, AuraAgent};
use ime_core::asr_buffer::AsrBuffer;
use ime_core::io_thread::IoThread;

/// Connectivity enum re-exported from the SDK (TUI matches on it for the status indicator).
pub use audio_aura_agent::AuraConn;

/// Default aura-daemon origin.
const DEFAULT_AURA: &str = "127.0.0.1:9091";
/// Event-drain cadence (the agent's state is authoritative; this only pushes into AsrBuffer).
const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

/// Aura-daemon connectivity, read by the TUI for display. The agent owns the probing — this
/// handle is a cheap read view into it.
#[derive(Debug, Clone)]
pub struct AuraConnHandle {
    agent: Arc<AuraAgent>,
}

impl AuraConnHandle {
    /// Current connectivity (best-effort, last known).
    pub fn get(&self) -> AuraConn {
        self.agent.conn()
    }
}

// ── Public API ─────────────────────────────────────────────────────────

/// Connect to aura via the SDK agent and wire its event drain into the engine's I/O thread.
///
/// - The agent's three driver tasks (health / segments / control plane) are spawned on
///   the engine's [`IoThread`] runtime via `spawn_into`(绕开 current_thread runtime 跨线程
///   `Handle::spawn` 不 poll 的问题)。
/// - A periodic drain task on the same runtime folds agent events into the shared
///   `AsrBuffer` (drain cadence [`DRAIN_INTERVAL`]).
///
/// Returns a connectivity handle the frontend can poll for display. The IME main thread
/// never blocks on network I/O (`AsrBuffer` is lock-guarded, only held microseconds per op).
///
/// `aura_addr` may be a bare `host:port` (http:// is prepended) or a full origin URL.
pub fn spawn_aura_client(
    buffer: Arc<AsrBuffer>,
    io: Arc<IoThread>,
    aura_addr: Option<&str>,
) -> AuraConnHandle {
    let base = normalize_origin(aura_addr.unwrap_or(DEFAULT_AURA));
    tracing::info!(addr = %base, "aura agent starting on engine I/O thread");

    let agent = Arc::new(
        AuraAgent::new(&base).unwrap_or_else(|e| panic!("aura agent init: {e}")),
    );

    // Drain + driver 三 task 都在 IoThread runtime 内通过 `spawn_into` 启动。
    // `factory` 在 main future 内被调,负责 `tokio::spawn(future)` 并把 JoinHandle
    // push 到 aux_tasks。Runtime drop / engine drop 时所有 task 一并 abort。
    let agent_for_start = Arc::clone(&agent);
    io.spawn_into(|| {
        let handle = tokio::runtime::Handle::current();
        let agent = agent_for_start;
        tokio::spawn(async move {
            agent.start(&handle).await;
        })
    });
    let drain_agent = Arc::clone(&agent);
    let drain_buf = Arc::clone(&buffer);
    io.spawn_into(|| {
        tokio::spawn(async move {
            loop {
                drain_buf.set_connected(matches!(drain_agent.conn(), AuraConn::Connected));
                for ev in drain_agent.poll_events() {
                    match ev {
                        AgentEvent::ConnChanged(c) => {
                            let connected = matches!(c, AuraConn::Connected);
                            drain_buf.set_connected(connected);
                            tracing::debug!(?c, "aura conn changed");
                        }
                        AgentEvent::StreamFragment { text, .. } => {
                            if !text.is_empty() {
                                drain_buf.set_live(&text);
                                tracing::debug!(text = %text, "asr live (stream)");
                            }
                        }
                        AgentEvent::SegmentCalibration { calibrated, .. } => {
                            if !calibrated.is_empty() {
                                drain_buf.set_live(&calibrated);
                                tracing::debug!(text = %calibrated, "asr live (segment calibration)");
                            }
                        }
                        AgentEvent::WindowCalibration(u)
                            if !u.calibrated.is_empty() => {
                                drain_buf.push_final(&u.calibrated);
                                tracing::info!(text = %u.calibrated, "asr final → candidate #1");
                            }
                        _ => {}
                    }
                }
                match drain_agent.live() {
                    Some((window_id, _)) => {
                        let plain = drain_agent.get_window_preview(window_id).unwrap_or_default();
                        let calc = drain_agent.get_window_calc_preview(window_id).unwrap_or_default();
                        drain_buf.set_preview(ime_core::asr_buffer::AsrPreview { window_id, plain, calc });
                    }
                    None => drain_buf.clear_preview(),
                }
                tokio::time::sleep(DRAIN_INTERVAL).await;
            }
        })
    });

    AuraConnHandle { agent }
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