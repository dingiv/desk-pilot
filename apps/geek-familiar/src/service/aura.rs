//! Minimal HTTP client for audio-aura daemon control endpoints.
//!
//! Uses raw `TcpStream` (same style as the SSE client in [asr]) so we don't pull
//! in `reqwest` or another async runtime.  All calls are short-lived and
//! blocking — they run inside `iced::Task::perform` on a background thread.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// Send a minimal HTTP/1.1 GET, return the stripped body.
fn get(addr: &str, path: &str) -> Result<String, String> {
    let mut stream = connect(addr)?;
    write_req(&mut stream, addr, "GET", path, None)?;
    read_body(&mut stream)
}

/// Send a minimal HTTP/1.1 POST with a JSON body, return the stripped body.
fn post_json(addr: &str, path: &str, json: &str) -> Result<String, String> {
    let mut stream = connect(addr)?;
    write_req(&mut stream, addr, "POST", path, Some(json))?;
    read_body(&mut stream)
}

fn connect(addr: &str) -> Result<TcpStream, String> {
    TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("bad addr: {e}"))?,
        Duration::from_secs(2),
    )
    .map_err(|e| format!("connect {addr}: {e}"))
}

fn write_req(stream: &mut TcpStream, addr: &str, method: &str, path: &str, body: Option<&str>) -> Result<(), String> {
    let body_str = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body_str}",
        body_str.len()
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))
}

fn read_body(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    let raw = String::from_utf8_lossy(&buf);
    // Split headers and body at the first double CRLF.
    raw.split("\r\n\r\n").nth(1).map(|s| s.trim().to_string()).ok_or_else(|| "no body".to_string())
}

// ── API helpers ──────────────────────────────────────────────────────────────

/// Check whether the daemon is reachable.
pub fn health(addr: &str) -> bool {
    get(addr, "/health").map(|r| r.contains("ok")).unwrap_or(false)
}

/// Toggle or set the scout/recording connection state.
/// `enabled` = `None` flips; `Some(v)` sets explicitly.
/// Returns the new `connected` state.
pub fn control_scout(addr: &str, enabled: Option<bool>) -> Result<bool, String> {
    let json = match enabled {
        Some(v) => format!("{{\"enabled\":{v}}}"),
        None => "{}".to_string(),
    };
    let resp = post_json(addr, "/api/control/scout", &json)?;
    let v: Value = serde_json::from_str(&resp).map_err(|e| format!("parse: {e}"))?;
    v.get("connected")
        .and_then(|c| c.as_bool())
        .ok_or_else(|| "missing 'connected' field".to_string())
}

/// Fetch recent turn results (up to 100) so the pet can pre-populate its
/// transcript on startup.
pub fn results(addr: &str) -> Result<Vec<TurnRecord>, String> {
    let resp = get(addr, "/results")?;
    let v: Value = serde_json::from_str(&resp).map_err(|e| format!("parse: {e}"))?;
    let records: Vec<TurnRecord> = serde_json::from_value(v["results"].clone())
        .map_err(|e| format!("parse results: {e}"))?;
    Ok(records)
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnRecord {
    pub seq: u64,
    #[serde(default)]
    pub raw_text: String,
    #[serde(default)]
    pub calibrated: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub reply: String,
}
