//! audio-aura SSE client — framework-agnostic.
//!
//! Connects to the aura daemon (`GET /api/stream`), parses live ASR turn events
//! (interim partials + final utterances + intents), and reports each as an
//! [`AsrUpdate`] through a caller-supplied callback. Reconnects every 2 s on
//! failure. Reports `Connected`/`Disconnected` on link transitions so the UI can
//! show status. Spawns a background thread; returns immediately.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

/// A transcript update reported from the SSE client thread.
#[derive(Debug, Clone)]
pub enum AsrUpdate {
    Interim(String),
    Final { text: String, intent: String },
    Connected,
    Disconnected,
}

/// Spawn the aura SSE loop on a background thread, invoking `on_update` per
/// event. `on_update` must be `Send + 'static` (it crosses the thread boundary).
pub fn spawn(addr: String, mut on_update: impl FnMut(AsrUpdate) + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("familiar-asr-sse".into())
        .spawn(move || {
            let req = b"GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n";
            loop {
                if let Ok(mut stream) = TcpStream::connect(&addr) {
                    let _ = stream.write_all(req);
                    on_update(AsrUpdate::Connected);
                    for line in BufReader::new(stream).lines().by_ref() {
                        let line = match line { Ok(l) => l, Err(_) => break };
                        if let Some(json) = line.strip_prefix("data: ") {
                            if let Ok(v) = serde_json::from_str::<Value>(json) {
                                if let Some(upd) = parse_sse_event(&v) {
                                    on_update(upd);
                                }
                            }
                        }
                    }
                    on_update(AsrUpdate::Disconnected);
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

/// Extract a transcript [`AsrUpdate`] from a TurnEvent JSON value (or `None`).
fn parse_sse_event(v: &Value) -> Option<AsrUpdate> {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "interim" => v
            .get("partial")
            .and_then(|p| p.as_str())
            .map(|s| AsrUpdate::Interim(s.to_string())),
        "final" => {
            // calibrated (Stage2 rewrite) is authoritative; fall back to raw ASR.
            let text = v
                .get("calibrated")
                .filter(|c| c.as_str().map_or(false, |s| !s.trim().is_empty()))
                .or_else(|| v.get("raw_text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let intent = v.get("intent").and_then(|i| i.as_str()).unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(AsrUpdate::Final { text: text.to_string(), intent: intent.to_string() })
            }
        }
        _ => None,
    }
}
