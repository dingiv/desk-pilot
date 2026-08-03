//! Cross-subsystem communication.
//!
//! - **aura SSE client**: connects to aura-daemon (:9091), subscribes to `/api/stream`,
//!   accumulates `AsrBuffer` for `#asr` voice insertion.
//! - **familiar TCP server**: listens on :9601, accepts familiar connections for snippet
//!   config push + status display (Phase 2 stub).
//!
//! Threading: the SSE client runs on a dedicated `std::thread` with its own tokio
//! runtime for async HTTP. Events (`final` type) update a shared [`AsrBuffer`]
//! that the fcitx5 keyEvent path reads with microsecond-latency Mutex access.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ime_core::asr_buffer::AsrBuffer;

/// Default aura-daemon SSE endpoint.
const AURA_SSE_URL: &str = "127.0.0.1:9091";

/// Spawn the aura SSE client on a background thread. It connects to
/// `aura_addr/api/stream`, parses `final` events, and updates `buffer`
/// with the calibrated text. Auto-reconnects on disconnect with a 2-second
/// back-off.
pub fn spawn_aura_sse(buffer: Arc<AsrBuffer>, aura_addr: Option<&str>) {
    let addr = aura_addr.unwrap_or(AURA_SSE_URL).to_string();
    tracing::info!(addr = %addr, "aura SSE client starting");

    thread::Builder::new()
        .name("ime-aura-sse".into())
        .spawn(move || {
            loop {
                match connect_and_stream(&addr, &buffer) {
                    Ok(()) => tracing::info!("aura SSE stream ended cleanly"),
                    Err(e) => tracing::warn!(error = %e, "aura SSE error — reconnecting in 2s"),
                }
                thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn aura SSE thread");
}

/// Open a raw TCP connection to the aura SSE endpoint and read events line
/// by line. On each `final` event the `calibrated` field is extracted and
/// written to the buffer.
///
/// Before entering the SSE loop, fetches `/results` to seed the buffer with
/// the most recent calibrated utterance (so `#asr` works immediately even
/// without new voice input).
fn connect_and_stream(addr: &str, buffer: &AsrBuffer) -> Result<(), Box<dyn std::error::Error>> {
    // ── Seed buffer from the most recent result ──
    if let Ok(text) = fetch_latest_calibrated(addr) {
        if !text.is_empty() {
            buffer.update(&text);
            tracing::info!(text, "asr buffer seeded from /results");
        }
    }

    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?; // 5 min idle → reconnect

    // Send the SSE handshake.
    write!(stream, "GET /api/stream HTTP/1.1\r\n")?;
    write!(stream, "Host: {addr}\r\n")?;
    write!(stream, "Accept: text/event-stream\r\n")?;
    write!(stream, "Connection: keep-alive\r\n")?;
    write!(stream, "\r\n")?;
    stream.flush()?;

    let reader = BufReader::new(stream);
    let mut data_buf = String::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();

        if trimmed.is_empty() {
            // Empty line = end of event. Process the accumulated data.
            if !data_buf.is_empty() {
                handle_event(&data_buf, buffer);
                data_buf.clear();
            }
            continue;
        }

        if let Some(data) = trimmed.strip_prefix("data: ") {
            data_buf.push_str(data);
        }
        // Ignore `event:`, `id:`, `:` (comment) lines.
    }

    Ok(())
}

/// Parse one SSE `data:` payload and update the buffer if it's a `final` event.
fn handle_event(data: &str, buffer: &AsrBuffer) {
    // The hello / status events are ignored here.
    let Ok(val) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };

    let Some(ev_type) = val.get("type").and_then(|v| v.as_str()) else {
        return;
    };

    match ev_type {
        "final" => {
            // Prefer calibrated (Stage2 rewrites), fall back to raw_text.
            let text = val
                .get("calibrated")
                .or_else(|| val.get("raw_text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !text.is_empty() {
                buffer.update(text);
                tracing::info!(text, "asr buffer updated");
            }
        }
        "interim" => {
            // Streaming partials are not consumed this round.
        }
        "hello" | "status" => {
            tracing::debug!(?ev_type, "sse event");
        }
        _ => {}
    }
}

/// ── /results seeding ─────────────────────────────────────────────────

/// Fetch the most recent calibrated text from aura's `/results` endpoint.
/// Used to seed the AsrBuffer on connect so `#asr` has data immediately.
fn fetch_latest_calibrated(addr: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| format!("timeout: {e}"))?;
    let req = format!("GET /results HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    let raw = String::from_utf8_lossy(&buf);
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").trim();

    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("parse: {e}"))?;
    let results = v.get("results").and_then(|r| r.as_array());

    // Find the latest result (highest seq) with non-empty calibrated text.
    let latest = results
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let text = r.get("calibrated")?.as_str()?;
            let seq = r.get("seq")?.as_u64()?;
            if text.is_empty() { None } else { Some((seq, text.to_string())) }
        })
        .max_by_key(|(seq, _)| *seq);

    Ok(latest.map(|(_, t)| t).unwrap_or_default())
}

/// ── Familiar TCP server (Phase 2 stub) ──

/// Spawn the familiar TCP server thread (Phase 2 stub).
pub fn spawn_familiar_server() {
    tracing::info!("familiar TCP server :9601 — stub (Phase 2)");
}
