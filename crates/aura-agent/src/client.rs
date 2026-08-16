//! client.rs — `AuraClient`: an async Rust SDK for the aura-daemon, with TWO streaming planes:
//!
//! - **Control plane** ([`subscribe`]): snapshot-sync. The daemon is the source of truth; this
//!   fetches the full [`AuraStateView`] and re-fetches on each throttled `state_changed` ping.
//!   Right for low-frequency state (connection, config, hotwords, corrections).
//! - **Data plane** ([`subscribe_segments`]): live recognition segments pushed directly
//!   (low-latency, every event). Each [`AsrSegment`] is one Interim / CalibratedInterim / Final —
//!   render the live streaming text off this, without the ping→fetch round-trip.
//!
//! Both streams are resilient + infinite (they reconnect on drop). This crate is dependency-light
//! on purpose (no mistralrs/asr) so an upper layer talks to aura without the GPU stack.
//!
//! ```ignore
//! use audio_aura_agent::client::AuraClient;
//! use futures::StreamExt;
//! # #[tokio::main] async fn main() -> anyhow::Result<()> {
//! let client = AuraClient::new("http://127.0.0.1:9091")?;
//! let segs = client.subscribe_segments();
//! tokio::pin!(segs);
//! while let Some(seg) = segs.next().await {
//!     println!("{seg:?}");
//! }
//! # Ok(()) }
//! ```

use std::time::Duration;

use anyhow::Result;
use futures::{Stream, StreamExt};

use crate::view::{AsrSegment, AuraStateView};

/// Reconnect backoff when an SSE stream drops or the daemon is unreachable.
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

    /// The daemon origin (normalized, no trailing slash) — the client's identity.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET /health` — true if the daemon is reachable.
    pub async fn health(&self) -> Result<bool> {
        let r = self.http.get(format!("{}/health", self.base)).send().await?;
        Ok(r.status().is_success())
    }

    /// `GET /api/state` — the complete snapshot (the control-plane source of truth).
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

    /// Raw `data:` payloads from an SSE endpoint, with reconnect. Shared by [`subscribe`] and
    /// [`subscribe_segments`]. Yields one owned String per `data:` line (a frame may carry ≥1).
    fn sse_data(&self, url: String) -> impl Stream<Item = String> + '_ {
        async_stream::stream! {
            loop {
                match self.http.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        let mut bytes = resp.bytes_stream();
                        let mut buf = String::new();
                        // Drive this connection until it ends/errors, then fall through to reconnect.
                        while let Some(Ok(chunk)) = bytes.next().await {
                            // SSE frames are ASCII (JSON pings / keep-alive comments) —
                            // lossy utf8 is safe and never splits a multi-byte char mid-frame.
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                            while let Some(idx) = buf.find("\n\n") {
                                let frame: String = buf.drain(..idx + 2).collect();
                                for payload in data_payloads(&frame) {
                                    yield payload.to_string();
                                }
                            }
                        }
                    }
                    Ok(resp) => tracing::warn!(status = %resp.status(), "aura SSE bad status; reconnecting"),
                    Err(e) => tracing::warn!(error = %e, "aura SSE connect failed; retrying"),
                }
                tokio::time::sleep(RECONNECT_BACKOFF).await;
            }
        }
    }

    /// **Control plane** — snapshot-sync. Opens `GET /api/stream?state_changed_frequency=<ms>` and,
    /// on each `state_changed` ping, fetches `/api/state` and yields the snapshot. Resilient +
    /// infinite (reconnects on drop). `freq_ms` is the min interval between pings (floored to 250ms
    /// by the server). Backpressure is cooperative: the consumer renders at its own pace.
    pub fn subscribe(&self, freq_ms: u64) -> impl Stream<Item = AuraStateView> + '_ {
        let url = format!("{}/api/stream?state_changed_frequency={}", self.base, freq_ms.max(250));
        async_stream::stream! {
            let data = self.sse_data(url);
            tokio::pin!(data); // sse_data's stream is !Unpin — pin before .next()
            while let Some(payload) = data.next().await {
                if payload.contains("\"state_changed\"") {
                    if let Ok(snap) = self.state().await {
                        yield snap;
                    }
                }
            }
        }
    }

    /// **Data plane** — live recognition segments pushed directly. Opens `GET /api/asr_stream` and
    /// yields each [`AsrSegment`] (Interim / CalibratedInterim / Final) as it happens —
    /// low-latency, every event, no ping→fetch round-trip. Resilient + infinite (reconnects on
    /// drop). Render the live streaming text off this.
    pub fn subscribe_segments(&self) -> impl Stream<Item = AsrSegment> + '_ {
        let url = format!("{}/api/asr_stream", self.base);
        async_stream::stream! {
            let data = self.sse_data(url);
            tokio::pin!(data);
            while let Some(payload) = data.next().await {
                if let Ok(seg) = serde_json::from_str::<AsrSegment>(&payload) {
                    yield seg;
                }
            }
        }
    }
}

/// The `data:` payloads of one SSE frame (the text between two blank lines). Each `data:` line's
/// content, trimmed (SSE allows an optional space after the colon).
fn data_payloads(frame: &str) -> impl Iterator<Item = &str> {
    frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::data_payloads;

    #[test]
    fn extracts_data_payloads() {
        let frame = "data: {\"type\":\"state_changed\"}\n";
        assert_eq!(data_payloads(frame).collect::<Vec<_>>(), vec!["{\"type\":\"state_changed\"}"]);
        // optional space after colon
        assert_eq!(data_payloads("data:  hi").collect::<Vec<_>>(), vec!["hi"]);
    }

    #[test]
    fn ignores_non_data_lines() {
        // keep-alive comments (axum KeepAlive) and event/id lines carry no data.
        assert_eq!(data_payloads(": keep-alive\n").collect::<Vec<_>>(), Vec::<&str>::new());
        assert_eq!(data_payloads("event: ping\ndata:\n").collect::<Vec<_>>(), Vec::<&str>::new());
    }
}
