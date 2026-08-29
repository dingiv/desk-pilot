//! upstream — 远程 OpenAI 兼容上游转发。
//!
//! dp-router 命中本地模型失败时,fallback 到此。`base_url` 为空 = 关闭。

use anyhow::Result;
use tracing::info;

use crate::config::RemoteUpstreamConfig;

#[derive(Clone)]
pub struct UpstreamClient {
    pub base_url: String,
    pub api_key: Option<String>,
    /// 缺省 `model` 字段时的兜底模型名(本轮未使用,留作后续)。
    #[allow(dead_code)]
    pub default_model: Option<String>,
    pub http: reqwest::Client,
}

impl UpstreamClient {
    pub fn new(cfg: RemoteUpstreamConfig, http: reqwest::Client) -> Option<Self> {
        if cfg.base_url.trim().is_empty() {
            None
        } else {
            Some(Self {
                base_url: cfg.base_url.trim_end_matches('/').to_string(),
                api_key: cfg.api_key,
                default_model: cfg.default_model,
                http,
            })
        }
    }

    pub fn is_enabled(&self) -> bool {
        !self.base_url.is_empty()
    }

    /// 拼出 `/v1/chat/completions` URL。
    pub fn chat_url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    #[allow(dead_code)]
    pub fn models_url(&self) -> String {
        format!("{}/v1/models", self.base_url)
    }

    /// 转发请求。返回 (status, content_type, body_bytes)。
    pub async fn forward_chat(
        &self,
        body: bytes::Bytes,
    ) -> Result<(u16, Option<String>, bytes::Bytes)> {
        let url = self.chat_url();
        info!("[dp-router] forwarding to upstream: {url}");
        let mut req = self.http.post(&url).header("content-type", "application/json").body(body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(String::from);
        let body = resp.bytes().await?;
        Ok((status, ct, body))
    }
}

/// 偷懒 alias:这个 crate 用 `bytes` 仅作 raw body 类型,避免拉 `axum::body::Bytes` 转换。
pub mod bytes {
    pub use ::bytes::Bytes;
}