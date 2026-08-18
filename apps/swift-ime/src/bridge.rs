//! Cross-subsystem communication.
//!
//! - **aura data-plane client**: the `audio-aura-agent` SDK's [`AuraAgent`] owns the whole
//!   connection — its background driver keeps the daemon link, reconnects, and probes health.
//!   This bridge only drains the agent's events (`poll_events`, non-blocking) and writes the
//!   calibrated text into the shared [`AsrBuffer`], which the `#asr` magic command reads
//!   (non-blocking, microseconds) on the IME key-event path. No tokio runtime, no health probe,
//!   no segment matching — all encapsulated in the agent.
//! - **familiar TCP server**: listens on :9601, accepts familiar connections for snippet config
//!   push + status display (Phase 2 stub).

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use audio_aura_agent::agent::{AgentEvent, AuraAgent};
use ime_core::asr_buffer::AsrBuffer;

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

/// Connect to aura via the SDK agent and drain its events into `buffer` on a background thread.
/// Returns a connectivity handle the frontend can poll for display. The agent's driver thread
/// keeps the connection alive + reconnects; this bridge thread never blocks the IME main thread
/// (`AsrBuffer` is lock-guarded and only held microseconds per op).
///
/// `aura_addr` may be a bare `host:port` (http:// is prepended) or a full origin URL.
pub fn spawn_aura_client(buffer: Arc<AsrBuffer>, aura_addr: Option<&str>) -> AuraConnHandle {
    let base = normalize_origin(aura_addr.unwrap_or(DEFAULT_AURA));
    tracing::info!(addr = %base, "aura agent starting");

    // AuraAgent::connect only fails on client construction (never on daemon reachability — the
    // driver discovers that asynchronously and reports Disconnected via the handle).
    let agent = Arc::new(
        AuraAgent::connect(&base).unwrap_or_else(|e| panic!("aura agent init: {e}")),
    );

    let drain_agent = Arc::clone(&agent);
    thread::Builder::new()
        .name("ime-aura-drain".into())
        .spawn(move || loop {
            // Push aura connectivity so `#asr` shows "语音不可用" while the daemon
            // is down / not yet connected (the member only surfaces voice data
            // when Connected). Initial read + `ConnChanged` events keep it reactive;
            // the per-iteration `conn()` read is a cheap (atomic) safety net so a
            // lagged broadcast consumer never stalls the flag.
            buffer.set_connected(matches!(drain_agent.conn(), AuraConn::Connected));
            for ev in drain_agent.poll_events() {
                match ev {
                    AgentEvent::ConnChanged(c) => {
                        let connected = matches!(c, AuraConn::Connected);
                        buffer.set_connected(connected);
                        tracing::debug!(?c, "aura conn changed");
                    }
                    // ① new Stage1 streaming fragment — raw live text.
                    AgentEvent::StreamFragment { text, .. } => {
                        if !text.is_empty() {
                            buffer.set_live(&text);
                            tracing::debug!(text = %text, "asr live (stream)");
                        }
                    }
                    // ④ Stage2 jointly calibrated the window (per Batch) — live correction.
                    AgentEvent::SegmentCalibration { calibrated, .. } => {
                        if !calibrated.is_empty() {
                            buffer.set_live(&calibrated);
                            tracing::debug!(text = %calibrated, "asr live (segment calibration)");
                        }
                    }
                    // ⑤ the window settled — authoritative calibrated text (also
                    // clears the agent's live; push_final does the same here).
                    AgentEvent::WindowCalibration(u)
                        if !u.calibrated.is_empty() => {
                            buffer.push_final(&u.calibrated);
                            tracing::info!(text = %u.calibrated, "asr final → candidate #1");
                        }
                    // ②③ 整窗 batch 重跑(BatchWindow 是权威 raw,但不作为 live ——
                    // agent 语义,校准紧跟其后)、StateChanged 快照、WindowCorrected
                    // 修正确认:都不是语音缓冲区输入,忽略。
                    _ => {}
                }
            }
            // 组装预览:当前活动窗口(live 存在)→ 用 aura-agent 的
            // `get_window_preview`(plain)+ `get_window_calc_preview`(calc)喂给
            // `#asr`/`#asr/calc`。窗口关闭后 live 清空 → 清预览,定稿已由
            // WindowCalibration 作为候选提供。
            match drain_agent.live() {
                Some((window_id, _)) => {
                    let plain = drain_agent.get_window_preview(window_id).unwrap_or_default();
                    let calc = drain_agent.get_window_calc_preview(window_id).unwrap_or_default();
                    buffer.set_preview(ime_core::asr_buffer::AsrPreview { window_id, plain, calc });
                }
                None => buffer.clear_preview(),
            }
            thread::sleep(DRAIN_INTERVAL);
        })
        .expect("spawn aura drain thread");

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
