//! client.rs — `AuraClient`: an async Rust SDK for the aura-daemon's snapshot-sync API.
//!
//! The daemon is the single source of truth; this client fetches the full [`AuraStateView`]
//! snapshot and re-fetches on change. For the live-updating case, [`AuraClient::subscribe`]
//! opens the SSE ping stream and yields a fresh snapshot on each `state_changed` ping (it
//! reconnects with backoff, so the stream is resilient — the consumer just renders snapshots).
//!
//! This crate (audio-aura-agent) is dependency-light on purpose — no mistralrs/asr — so an upper
//! layer (the desktop-pet secretary, visual-rover, …) can talk to aura without pulling the GPU
//! inference stack.
//!
//! ```ignore
//! use audio_aura_agent::client::AuraClient;
//! use futures::StreamExt;
//! # #[tokio::main] async fn main() -> anyhow::Result<()> {
//! let client = AuraClient::new("http://127.0.0.1:9091")?;
//! let states = client.subscribe(400); // ping ≥400ms
//! tokio::pin!(states);
//! while let Some(snap) = states.next().await {
//!     println!("{} utterances, connected={}", snap.utterances.len(), snap.connected);
//! }
//! # Ok(()) }
//! ```

use std::time::Duration;

use anyhow::Result;
use futures::{Stream, StreamExt};

use crate::view::AuraStateView;

/// Reconnect backoff when the SSE stream drops or the daemon is unreachable.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Async client for the aura-daemon HTTP socket. Cheap to clone (shares the reqwest pool).
#[derive(Clone)]
pub struct AuraClient {
    base: String,
    http: reqwest::Client,
}

impl AuraClient {
    /// `base` is the daemon origin, e.g. `http://127.0.0.1:9091` (trailing slash trimmed).
    pub fn new(base: impl Into<String>) -> Result<Self> {
        let base = base.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        Ok(Self { base, http })
    }

    /// `GET /health` — true if the daemon is reachable.
    pub async fn health(&self) -> Result<bool> {
        let r = self.http.get(format!("{}/health", self.base)).send().await?;
        Ok(r.status().is_success())
    }

    /// `GET /api/state` — the complete snapshot (the one source of truth).
    pub async fn state(&self) -> Result<AuraStateView> {
        Ok(self
            .http
            .get(format!("{}/api/state", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// `POST /api/control/scout {enabled}` — toggle aura's own scout connection (does NOT kill
    /// scout). Returns the new connected state the daemon reports.
    pub async fn set_connected(&self, enabled: bool) -> Result<bool> {
        let v: serde_json::Value = self
            .http
            .post(format!("{}/api/control/scout", self.base))
            .json(&serde_json::json!({ "enabled": enabled }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("connected").and_then(|x| x.as_bool()).unwrap_or(enabled))
    }

    /// `POST /api/correct {seq, raw, corrected}` — record a user correction (feeds Stage2).
    pub async fn correct(&self, seq: u64, raw: &str, corrected: &str) -> Result<()> {
        self.http
            .post(format!("{}/api/correct", self.base))
            .json(&serde_json::json!({ "seq": seq, "raw": raw, "corrected": corrected }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// `GET /api/audio/{seq}` — the utterance's WAV bytes (for playback).
    pub async fn audio(&self, seq: u64) -> Result<Vec<u8>> {
        Ok(self
            .http
            .get(format!("{}/api/audio/{}", self.base, seq))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec())
    }

    /// `GET /api/recordings` — seq numbers of all known clips (hot + flushed), ascending.
    pub async fn recordings(&self) -> Result<Vec<u64>> {
        let v: serde_json::Value = self
            .http
            .get(format!("{}/api/recordings", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("recordings")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
            .unwrap_or_default())
    }

    /// Subscribe to state changes: opens `GET /api/stream?state_changed_frequency=<ms>` and, on
    /// each `state_changed` ping, fetches `/api/state` and yields the snapshot. The stream is
    /// **resilient + infinite** — on any drop (daemon restart, network blip) it reconnects after
    /// [`RECONNECT_BACKOFF`]. `freq_ms` is the min interval between pings (floored to 250ms by the
    /// server). Backpressure is cooperative: when the consumer stops pulling, no pings are
    /// processed (it just renders at its own pace).
    pub fn subscribe(&self, freq_ms: u64) -> impl Stream<Item = AuraStateView> + '_ {
        let freq = freq_ms.max(250);
        let url = format!("{}/api/stream?state_changed_frequency={}", self.base, freq);
        async_stream::stream! {
            loop {
                match self.http.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        let mut bytes = resp.bytes_stream();
                        let mut buf = String::new();
                        // Drive this connection until it ends/errors, then fall through to reconnect.
                        loop {
                            match bytes.next().await {
                                Some(Ok(chunk)) => {
                                    // SSE frames are ASCII (`{"type":"state_changed"}` / keep-alive
                                    // comments) — lossy utf8 is safe and never splits a multi-byte
                                    // char mid-frame.
                                    buf.push_str(&String::from_utf8_lossy(&chunk));
                                    while let Some(idx) = buf.find("\n\n") {
                                        let frame: String = buf.drain(..idx + 2).collect();
                                        if frame_is_state_changed(&frame) {
                                            if let Ok(snap) = self.state().await {
                                                yield snap;
                                            }
                                        }
                                    }
                                }
                                _ => break, // chunk error or clean end → reconnect
                            }
                        }
                    }
                    Ok(resp) => tracing::warn!(status = %resp.status(), "aura /api/stream bad status; reconnecting"),
                    Err(e) => tracing::warn!(error = %e, "aura /api/stream connect failed; retrying"),
                }
                tokio::time::sleep(RECONNECT_BACKOFF).await;
            }
        }
    }
}

/// Does an SSE frame (the text between two blank lines) carry a `state_changed` data payload?
fn frame_is_state_changed(frame: &str) -> bool {
    frame.lines().any(|line| {
        let data = line.strip_prefix("data:").unwrap_or("").trim();
        if data.is_empty() {
            return false;
        }
        serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|s| s == "state_changed"))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::frame_is_state_changed;

    #[test]
    fn parses_state_changed_frame() {
        assert!(frame_is_state_changed("data: {\"type\":\"state_changed\"}\n"));
        assert!(frame_is_state_changed("data:{\"type\":\"state_changed\"}"));
    }

    #[test]
    fn ignores_other_frames() {
        assert!(!frame_is_state_changed("data: {\"type\":\"hello\"}\n"));
        assert!(!frame_is_state_changed(": keep-alive comment\n")); // axum KeepAlive
        assert!(!frame_is_state_changed(""));
    }
}
