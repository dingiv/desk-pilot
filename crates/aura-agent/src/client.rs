//! client.rs — `AuraClient`: an async Rust SDK for the aura-daemon, with TWO streaming planes:
//!
//! - **Control plane** ([`subscribe`]): snapshot-sync. The daemon is the source of truth; this
//!   fetches the full [`AuraStateView`] and re-fetches on each throttled `state_changed` ping.
//!   Right for low-frequency state (connection, config, hotwords, corrections).
//! - **Data plane** ([`subscribe_segments`]): live recognition segments pushed directly
//!   (low-latency, every event). Each [`AsrSegment`] is one of the five recognition events
//!   (StreamFragment / BatchSegment / BatchWindow / SegmentCalibration / WindowCalibration) —
//!   render the live text off this, without the ping→fetch round-trip.
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

/// SSE 重连的指数退避基准(1s → 2s → 4s → …,封顶 [`RECONNECT_MAX_DELAY`])。
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);
/// 连续连接失败的上限:超过即**放弃**,流结束(返回 `None`)—— 上层(voice
/// server)据此丢源、不再 select;下次 `#asr` 重新 `subscribe_segments_owned()`
/// 即"手动重连"(计数清零,重新走一遍退避)。
const RECONNECT_MAX_ATTEMPTS: u32 = 10;
/// 单次退避 sleep 的上限。
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

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

    /// `POST /api/correct {window_id, raw, corrected}` — record a user correction for a
    /// window (feeds Stage2).
    pub async fn correct(&self, window_id: u64, raw: &str, corrected: &str) -> Result<()> {
        self.http
            .post(format!("{}/api/correct", self.base))
            .json(&serde_json::json!({ "window_id": window_id, "raw": raw, "corrected": corrected }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// `GET /api/audio/{window_id}` — the settled window's WAV bytes (for playback).
    pub async fn audio(&self, window_id: u64) -> Result<Vec<u8>> {
        Ok(self
            .http
            .get(format!("{}/api/audio/{}", self.base, window_id))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec())
    }

    /// `GET /api/recordings` — window ids of all known clips (hot + flushed), ascending.
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
            let mut attempts: u32 = 0;
            loop {
                match self.http.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        // 连上过 → 计数清零,之后的断线重连从头退避。
                        attempts = 0;
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
                    Ok(resp) => {
                        tracing::warn!(status = %resp.status(), attempts, "aura SSE bad status");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, attempts, "aura SSE connect failed");
                    }
                }
                // 指数退避 + 上限:连续失败超过 RECONNECT_MAX_ATTEMPTS 次 → 放弃,
                // 流结束(返回 None)。下次 #asr 重新 subscribe = 手动重连。
                attempts += 1;
                if attempts >= RECONNECT_MAX_ATTEMPTS {
                    tracing::warn!(attempts, "aura SSE reconnect giving up (manual reconnect on next #asr)");
                    break;
                }
                let exp = 1u32 << attempts.min(6); // 2^attempts,封顶 64
                let delay = RECONNECT_BASE_DELAY
                    .saturating_mul(exp)
                    .min(RECONNECT_MAX_DELAY);
                tracing::debug!(attempts, ?delay, "aura SSE reconnect backoff");
                tokio::time::sleep(delay).await;
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
    /// yields each [`AsrSegment`] (StreamFragment / BatchSegment / BatchWindow /
    /// SegmentCalibration / WindowCalibration) as it happens — low-latency, every event, no
    /// ping→fetch round-trip. Resilient + infinite (reconnects on drop). Render the live
    /// streaming text off this.
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

    /// Same as [`subscribe_segments`], but consumes `self` so the returned stream **owns** its
    /// client and is `'static` — usable across scopes, e.g. moved into a `FuturesUnordered` in
    /// the IoThread's voice server. Reconnect / resilient behavior is unchanged.
    pub fn subscribe_segments_owned(self) -> impl Stream<Item = AsrSegment> + 'static {
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
