//! client.rs — `AuraClient`: an async Rust SDK for the aura-daemon, with TWO streaming planes:
//!
//! - **Control plane** ([`subscribe`]): snapshot-sync. The daemon is the source of truth; this
//!   fetches the full [`AuraStateView`] and re-fetches on each throttled `state_changed` ping.
//!   Right for low-frequency state (connection, config, hotwords, corrections).
//! - **Data plane** ([`subscribe_events`]): live recognition segments pushed directly
//!   (low-latency, every event). Each [`AsrEvent`] is one of the five recognition events
//!   (StreamFragment / BatchSentence / BatchParagraph / SentenceCalibration / ParagraphCalibration) —
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
//! let segs = client.subscribe_events();
//! tokio::pin!(segs);
//! while let Some(seg) = segs.next().await {
//!     println!("{seg:?}");
//! }
//! # Ok(()) }
//! ```

use std::time::Duration;

use anyhow::Result;
use futures::{Stream, StreamExt};

use crate::view::{AsrEvent, AuraStateView};

/// SSE 重连的指数退避基准(1s → 2s → 4s → …,封顶 [`RECONNECT_MAX_DELAY`])。
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);
/// 连续连接失败的上限:超过即**放弃**,流结束(返回 `None`)—— 上层(voice
/// server)据此丢源、不再 select;下次 `#asr` 重新 `subscribe_events_owned()`
/// 即"手动重连"(计数清零,重新走一遍退避)。
const RECONNECT_MAX_ATTEMPTS: u32 = 10;
/// 单次退避 sleep 的上限。
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

/// SSE 流的连接状态(供上层实时感知,及时汇报 UI)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseConnState {
    /// 正在尝试连接(退避重连中)。
    Connecting,
    /// 已连上(HTTP 200,数据流活着)。
    Connected,
    /// 本次连接尝试失败(bad status / 网络错误)—— 流内部仍在退避重试,
    /// 但上层应**立即**把 UI 切到"不可用",而不是等流彻底结束。
    Failed,
}

/// Async client for the aura-daemon HTTP socket. Cheap to clone (shares the reqwest pool).
#[derive(Clone)]
pub struct AuraClient {
    base: String,
    /// REST 调用(`state`/`results`/…,一次性请求):总超时 30s 合理。
    http: reqwest::Client,
    /// SSE 长流:**绝不设总超时** —— reqwest 的 `.timeout()` 覆盖整个响应生命周期,
    /// 会把流在 30s 处掐断(实测:每 ~30s 重连一次,断连窗口的事件永久丢失 +
    /// UI 闪"语音服务暂不可用")。只设连接超时;读等待无限(SSE 可以长时间无数据)。
    stream_http: reqwest::Client,
}

impl AuraClient {
    /// `base` is the daemon origin, e.g. `http://127.0.0.1:9091` (trailing slash trimmed).
    pub fn new(base: impl Into<String>) -> Result<Self> {
        let base = base.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        let stream_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { base, http, stream_http })
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

    /// `POST /api/control/flush` — 主动归档:让 aura 立即关闭当前开放段落并整段
    /// batch(IME 分字符 `'` = "我说完了"信号)。置位即返,不等识别结果 —— 结果
    /// 走既有 SSE 数据面(/api/asr_stream)推送。
    pub async fn flush_paragraph(&self) -> Result<()> {
        self.http
            .post(format!("{}/api/control/flush", self.base))
            .json(&serde_json::json!({}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// `POST /api/correct {window_id, raw, corrected}` — record a user correction for a
    /// paragraph (feeds Stage2; wire key `window_id` frozen).
    pub async fn correct(&self, paragraph_id: u64, raw: &str, corrected: &str) -> Result<()> {
        self.http
            .post(format!("{}/api/correct", self.base))
            .json(&serde_json::json!({ "window_id": paragraph_id, "raw": raw, "corrected": corrected }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// `GET /api/audio/{window_id}` — the settled paragraph's WAV bytes (for playback).
    pub async fn audio(&self, paragraph_id: u64) -> Result<Vec<u8>> {
        Ok(self
            .http
            .get(format!("{}/api/audio/{}", self.base, paragraph_id))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec())
    }

    /// `GET /api/recordings` — paragraph ids of all known clips (hot + flushed), ascending.
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

    /// `GET /api/results` — 最近定稿的识别文本(最旧 → 最新),重连后全量同步用。
    /// 数据面 `/api/asr_stream` 是 append-only broadcast,新订阅者收不到历史段;
    /// 此接口补足。返回 `(paragraph_id, calibrated)` 对(可空校准文本)。
    pub async fn results(&self) -> Result<Vec<(u64, String)>> {
        let v: serde_json::Value = self
            .http
            .get(format!("{}/api/results", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v.get("results")
            .and_then(|r| r.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let id = x.get("window_id").and_then(|i| i.as_u64())?;
                        let cal = x
                            .get("calibrated")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some((id, cal))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Raw `data:` payloads from an SSE endpoint, with reconnect. Shared by [`subscribe`] and
    /// [`subscribe_events`]. Yields one owned String per `data:` line (a frame may carry ≥1).
    ///
    /// `on_conn`(可选):每次连接状态变化时调用(Connecting / Connected / Failed),
    /// 让上层**及时**感知"连不上"—— 即使流内部还在退避重试,UI 也该立刻切不可用。
    fn sse_data(
        &self,
        url: String,
        on_conn: Option<Box<dyn Fn(SseConnState) + Send + Sync>>,
        on_reconnect: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> impl Stream<Item = String> + '_ {
        async_stream::stream! {
            let mut attempts: u32 = 0;
            let mut ever_connected = false;
            loop {
                if let Some(cb) = &on_conn {
                    cb(SseConnState::Connecting);
                }
                match self.stream_http.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        // 连上过 → 计数清零,之后的断线重连从头退避。
                        attempts = 0;
                        // **重连成功(非首次)**→ 通知上层对账:广播无回放,断连
                        // 窗口的事件已永久丢失,上层用 `/api/results` 全量补齐
                        //(round19;SSE 专用 client 修复 30s 掐流后,仅真实网络
                        // 断连会走到这里)。
                        if ever_connected {
                            if let Some(cb) = &on_reconnect {
                                cb();
                            }
                        }
                        ever_connected = true;
                        if let Some(cb) = &on_conn {
                            cb(SseConnState::Connected);
                        }
                        let mut bytes = resp.bytes_stream();
                        let mut buf: Vec<u8> = Vec::new();
                        // Drive this connection until it ends/errors, then fall through to reconnect.
                        while let Some(Ok(chunk)) = bytes.next().await {
                            // 字节级缓冲:**完整帧**才做 UTF-8 解码。识别文本是
                            // 中文(多字节),TCP 分片会把字符拦腰斩断 —— 逐
                            // chunk lossy 解码会把半截字符变 U+FFFD(显示缺字)。
                            // 帧分隔 `\n\n` 是 ASCII(0x0A),不可能出现在多字节
                            // 序列中间(续字节 ≥ 0x80),字节级搜索安全。
                            buf.extend_from_slice(&chunk);
                            while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
                                let frame: Vec<u8> = buf.drain(..idx + 2).collect();
                                let frame = String::from_utf8_lossy(&frame);
                                // broadcast 欠载标记(round11 审计):server 丢帧后
                                // 只发 `: lagged` comment —— 若不留痕,客户端根本
                                // 不知道中间缺了事件(苦等"永远不来的结束事件")。
                                if frame.lines().any(|l| l.trim_start().starts_with(": lagged")) {
                                    tracing::warn!("aura SSE lagged — server 丢弃了积压事件,本地序列有空洞");
                                }
                                for payload in data_payloads(&frame) {
                                    yield payload.to_string();
                                }
                            }
                        }
                    }
                    Ok(resp) => {
                        tracing::warn!(status = %resp.status(), attempts, "aura SSE bad status");
                        if let Some(cb) = &on_conn {
                            cb(SseConnState::Failed);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, attempts, "aura SSE connect failed");
                        if let Some(cb) = &on_conn {
                            cb(SseConnState::Failed);
                        }
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
            let data = self.sse_data(url, None, None);
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
    /// yields each [`AsrEvent`] (StreamFragment / BatchSentence / BatchParagraph /
    /// SentenceCalibration / ParagraphCalibration) as it happens — low-latency, every event, no
    /// ping→fetch round-trip. Resilient + infinite (reconnects on drop). Render the live
    /// streaming text off this.
    pub fn subscribe_events(&self) -> impl Stream<Item = AsrEvent> + '_ {
        let url = format!("{}/api/asr_stream", self.base);
        async_stream::stream! {
            let data = self.sse_data(url, None, None);
            tokio::pin!(data);
            while let Some(payload) = data.next().await {
                match parse_event(&payload) {
                    Some(seg) => yield seg,
                    None => continue,
                }
            }
        }
    }

    /// Same as [`subscribe_events`], but consumes `self` so the returned stream **owns** its
    /// client and is `'static` — usable across scopes, e.g. moved into a `FuturesUnordered` in
    /// the IoThread's voice server. Reconnect / resilient behavior is unchanged.
    pub fn subscribe_events_owned(self) -> impl Stream<Item = AsrEvent> + 'static {
        self.subscribe_events_owned_with_conn(None, None)
    }

    /// Owned data-plane stream + 连接状态回调(`on_conn` 每次 Connecting /
    /// Connected / Failed 时调用)—— voice server 用它把「连不上」及时汇报到
    /// UI,不必等退避重连到上限、流彻底结束。
    pub fn subscribe_events_owned_with_conn(
        self,
        on_conn: Option<Box<dyn Fn(SseConnState) + Send + Sync>>,
        on_reconnect: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> impl Stream<Item = AsrEvent> + 'static {
        let url = format!("{}/api/asr_stream", self.base);
        async_stream::stream! {
            let data = self.sse_data(url, on_conn, on_reconnect);
            tokio::pin!(data);
            while let Some(payload) = data.next().await {
                match parse_event(&payload) {
                    Some(seg) => yield seg,
                    None => continue,
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

/// 解析一个 data payload 为 [`AsrEvent`]。解析失败**不静默**:round11 审计
/// 发现 `if let Ok` 会把不匹配的事件(unknown tag / wire 字段变化)无声扔掉
/// —— 前端苦等 sentence/paragraph 结果事件的头号嫌疑。丢弃必须留痕。
///
/// **接收留痕(round18)**:每一条成功解析的事件先记一条 info 再返回 —— 与
/// server 端 `emit→前端`(pipeline.rs `describe_turn`)同词汇同格式,两边日志
/// 直接 diff 即可定位"server 发了 / 前端没收到"的丢事件缺口(lagged/断连/折叠丢弃)。
fn parse_event(payload: &str) -> Option<AsrEvent> {
    match serde_json::from_str::<AsrEvent>(payload) {
        Ok(seg) => {
            tracing::info!(event = %describe_event(&seg), "前端←event");
            Some(seg)
        }
        Err(e) => {
            // daemon 的连接握手 ack(`{"type":"hello"}`)不是 wire 识别事件 —— 静默
            // 跳过,不当"契约不匹配"报(每次重连一条,曾经的噪音源)。
            let is_hello = serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|t| t == "hello"))
                .unwrap_or(false);
            if is_hello {
                tracing::debug!("aura SSE 握手(hello),跳过");
                return None;
            }
            let head: String = payload.chars().take(120).collect();
            tracing::warn!(error = %e, payload = %head, "aura SSE 事件解析失败,已丢弃(wire 契约不匹配?)");
            None
        }
    }
}

/// 单行事件摘要(**接收侧**),与 server 端 `describe_turn`(pipeline.rs)同词汇:
/// `stream/batch_sentence/...` + `p<id> s<id>` + 文本 —— 对表 diff 时按
/// `p<id>` 与文本即可对齐(校准行不带 route_ms,server 侧多一个毫秒数)。
fn describe_event(ev: &AsrEvent) -> String {
    match ev {
        AsrEvent::StreamFragment { paragraph_id, sentence_id, text, at_s } => {
            format!("stream p{paragraph_id} s{sentence_id} @{at_s:.2} {text:?}")
        }
        AsrEvent::ParagraphClosed { paragraph_id } => {
            format!("paragraph_closed p{paragraph_id}")
        }
        AsrEvent::BatchSentence { paragraph_id, sentence_id, text } => {
            format!("batch_sentence p{paragraph_id} s{sentence_id} {text:?}")
        }
        AsrEvent::BatchParagraph { paragraph_id, text } => {
            format!("batch_paragraph p{paragraph_id} {text:?}")
        }
        AsrEvent::SentenceCalibration { paragraph_id, sentence_id, calibrated } => {
            format!("sentence_calibration p{paragraph_id} s{sentence_id} {calibrated:?}")
        }
        AsrEvent::ParagraphCalibration { paragraph_id, calibrated } => {
            format!("paragraph_calibration p{paragraph_id} {calibrated:?}")
        }
        AsrEvent::Correction { paragraph_id, raw, corrected } => {
            format!("correction p{paragraph_id} {raw:?}→{corrected:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{data_payloads, describe_event, parse_event};
    use crate::view::AsrEvent as E;

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

    /// describe_event(接收留痕)格式快照 —— 与 server `describe_turn` 同词汇,
    /// 对表 diff 的契约。
    #[test]
    fn describe_event_matches_server_vocabulary() {
        use crate::view::AsrEvent as E;
        assert_eq!(
            describe_event(&E::ParagraphClosed { paragraph_id: 7 }),
            "paragraph_closed p7"
        );
        assert!(describe_event(&E::BatchSentence {
            paragraph_id: 7, sentence_id: 2, text: "批".into(),
        }).starts_with("batch_sentence p7 s2"));
        assert!(describe_event(&E::StreamFragment {
            paragraph_id: 7, sentence_id: 2, text: "流".into(), at_s: 1.5,
        }).starts_with("stream p7 s2 @1.50"));
    }

    /// daemon 的 SSE 握手 ack(`{"type":"hello"}`)不是识别事件 —— 静默跳过
    /// (round19:曾每次重连触发一条"契约不匹配"warn 噪音)。
    #[test]
    fn hello_handshake_event_is_silently_skipped() {
        assert!(parse_event(r#"{"type":"hello"}"#).is_none());
        // 正常事件不受影响;文本里恰含 "hello" 字样不得误伤。
        let ev = parse_event(
            r#"{"type":"stream_fragment","window_id":1,"segment_id":1,"text":"hello world","at_s":0.5}"#,
        )
        .expect("text 含 hello 的正常事件必须解析成功");
        match ev {
            E::StreamFragment { text, .. } => assert_eq!(text, "hello world"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// 回归(round11 Bug 1):中文跨 TCP chunk 撕裂不得产生 U+FFFD。
    /// 逐字节喂入 —— 任何"逐 chunk 解码"的实现都会把多字节字符斩断。
    #[test]
    fn multibyte_text_split_across_chunks_survives() {
        let json = serde_json::json!({
            "type": "stream_fragment",
            "window_id": 1,
            "segment_id": 1,
            "text": "你好世界,语音识别文本",
            "at_s": 1.5
        })
        .to_string();
        let frame = format!("data: {json}\n\n");
        let bytes = frame.as_bytes();

        // 逐字节过一遍与 sse_data 相同的分帧逻辑,重组 payload。
        let mut buf: Vec<u8> = Vec::new();
        let mut payloads = Vec::new();
        for b in bytes {
            buf.push(*b);
            while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
                let frame: Vec<u8> = buf.drain(..idx + 2).collect();
                let frame = String::from_utf8_lossy(&frame);
                payloads.extend(data_payloads(&frame).map(str::to_string));
            }
        }
        assert_eq!(payloads.len(), 1);
        let ev: E = serde_json::from_str(&payloads[0]).expect("valid AsrEvent json");
        match ev {
            E::StreamFragment { text, .. } => assert_eq!(text, "你好世界,语音识别文本"),
            other => panic!("wrong event: {other:?}"),
        }
        assert!(!payloads[0].contains('\u{FFFD}'), "no replacement chars");
    }
}
