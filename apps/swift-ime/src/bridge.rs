//! Cross-subsystem communication.
//!
//! - **aura SSE client**: connects to aura-daemon (:9091). On startup fetches
//!   `GET /api/state` to seed the voice buffer. Then subscribes to
//!   `GET /api/stream?state_changed_frequency=<ms>` for `state_changed` pings,
//!   re-fetching `/api/state` on each ping to update the buffer.
//! - **familiar TCP server**: listens on :9601, accepts familiar connections for
//!   snippet config push + status display (Phase 2 stub).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ime_core::asr_buffer::AsrBuffer;

const AURA_ADDR: &str = "127.0.0.1:9091";
const STATE_CHANGED_FREQ: u64 = 500; // ms between pings (floor 250ms server-side)

// ── Public API ─────────────────────────────────────────────────────────

/// Spawn the aura SSE client on a background thread.
pub fn spawn_aura_sse(buffer: Arc<AsrBuffer>, aura_addr: Option<&str>) {
    let addr = aura_addr.unwrap_or(AURA_ADDR).to_string();
    tracing::info!(addr = %addr, "aura SSE client starting (snapshot-sync)");

    thread::Builder::new()
        .name("ime-aura-sse".into())
        .spawn(move || {
            loop {
                match sync_with_aura(&addr, &buffer) {
                    Ok(()) => tracing::info!("aura sync ended cleanly"),
                    Err(e) => tracing::warn!(error = %e, "aura sync error — reconnecting in 2s"),
                }
                thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn aura SSE thread");
}

// ── Snapshot-sync protocol ─────────────────────────────────────────────

fn sync_with_aura(addr: &str, buffer: &AsrBuffer) -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Seed the buffer with the current state ──
    if let Ok(text) = fetch_latest_calibrated(addr) {
        if !text.is_empty() {
            buffer.update(&text);
            tracing::info!(text, "asr buffer seeded from /api/state");
        }
    }

    // ── 2. Subscribe to state_changed pings ──
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;

    let path = format!("/api/stream?state_changed_frequency={STATE_CHANGED_FREQ}");
    write!(stream, "GET {path} HTTP/1.1\r\n")?;
    write!(stream, "Host: {addr}\r\n")?;
    write!(stream, "Accept: text/event-stream\r\n")?;
    write!(stream, "Connection: keep-alive\r\n\r\n")?;
    stream.flush()?;

    let reader = BufReader::new(stream);
    let mut data_buf = String::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();

        if trimmed.is_empty() {
            // End of SSE frame.
            if data_buf.contains("state_changed") {
                if let Ok(text) = fetch_latest_calibrated(addr) {
                    if !text.is_empty() {
                        buffer.update(&text);
                        tracing::info!(text, "asr buffer updated via state_changed");
                    }
                }
            }
            data_buf.clear();
            continue;
        }

        if let Some(data) = trimmed.strip_prefix("data:") {
            data_buf.push_str(data.trim());
        }
    }

    Ok(())
}

/// `GET /api/state` → extract latest utterance's calibrated text.
fn fetch_latest_calibrated(addr: &str) -> Result<String, String> {
    let body = http_get(addr, "/api/state")?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("parse /api/state: {e}"))?;

    let utterances = v.get("utterances").and_then(|u| u.as_array());
    // Find the latest utterance with a settled final calibrated text.
    let latest = utterances
        .into_iter()
        .flatten()
        .filter_map(|u| {
            let seq = u.get("seq")?.as_u64()?;
            let final_view = u.get("final")?;
            let calibrated = final_view.get("calibrated")?.as_str()?;
            if calibrated.is_empty() { None }
            else { Some((seq, calibrated.to_string())) }
        })
        .max_by_key(|(seq, _)| *seq);

    Ok(latest.map(|(_, t)| t).unwrap_or_default())
}

/// Minimal HTTP GET, return stripped body.
fn http_get(addr: &str, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| format!("timeout: {e}"))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    let raw = String::from_utf8_lossy(&buf);
    raw.split("\r\n\r\n").nth(1).map(|s| s.trim().to_string()).ok_or_else(|| "no body".to_string())
}

// ── Familiar TCP server (Phase 2 stub) ────────────────────────────────

pub fn spawn_familiar_server() {
    tracing::info!("familiar TCP server :9601 — stub (Phase 2)");
}
