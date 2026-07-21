//! OpenAI 兼容的 remote provider 实现 (`reqwest::blocking`, 同步)。
//!
//! 适配 vLLM / SGLang / qwen3-asr-rs `asr-server` / 任意 OpenAI 兼容服务。

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::{AsrProvider, LlmProvider, VlmProvider};

/// ASR via OpenAI `/v1/audio/transcriptions` (multipart wav)。
pub struct HttpAsr {
    client: reqwest::blocking::Client,
    endpoint: String,
}

impl HttpAsr {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self { client: reqwest::blocking::Client::new(), endpoint: endpoint.into() }
    }
}

impl AsrProvider for HttpAsr {
    fn recognize(&self, pcm: &[i16], sr: u32) -> Result<String> {
        let wav = pcm_to_wav(pcm, sr);
        let part = reqwest::blocking::multipart::Part::bytes(wav)
            .file_name("audio.wav")
            .mime_str("audio/wav")?;
        let form = reqwest::blocking::multipart::Form::new().part("file", part);
        let resp: TranscriptionResp = self
            .client
            .post(url(&self.endpoint, "/v1/audio/transcriptions"))
            .multipart(form)
            .send()?
            .error_for_status()?
            .json()?;
        Ok(resp.text)
    }
}

/// LLM via OpenAI `/v1/chat/completions`。
pub struct HttpLlm {
    client: reqwest::blocking::Client,
    endpoint: String,
    model: String,
}

impl HttpLlm {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::blocking::Client::new(),
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }
}

impl LlmProvider for HttpLlm {
    fn complete(&self, system: &str, user: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let resp: ChatResp = self
            .client
            .post(url(&self.endpoint, "/v1/chat/completions"))
            .json(&body)
            .send()?
            .error_for_status()?
            .json()?;
        Ok(resp.choices.into_iter().next().ok_or_else(|| anyhow!("no choices in response"))?.message.content)
    }
}

/// VLM via OpenAI `/v1/chat/completions` (image as `data:image/png;base64,...` URL)。
pub struct HttpVlm {
    client: reqwest::blocking::Client,
    endpoint: String,
    model: String,
}

impl HttpVlm {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::blocking::Client::new(),
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }
}

impl VlmProvider for HttpVlm {
    fn complete(&self, system: &str, user: &str, image_png: &[u8]) -> Result<String> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(image_png);
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": [
                    {"type": "text", "text": user},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}},
                ]},
            ],
        });
        let resp: ChatResp = self
            .client
            .post(url(&self.endpoint, "/v1/chat/completions"))
            .json(&body)
            .send()?
            .error_for_status()?
            .json()?;
        Ok(resp.choices.into_iter().next().ok_or_else(|| anyhow!("no choices in response"))?.message.content)
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// Encode PCM i16 mono → in-memory WAV bytes (16-bit PCM, no external dep).
fn pcm_to_wav(pcm: &[i16], sr: u32) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    // RIFF header
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    // fmt chunk (PCM, mono, 16-bit)
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // audio_format = PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // num_channels = mono
    w.extend_from_slice(&sr.to_le_bytes());
    w.extend_from_slice(&(sr * 2).to_le_bytes()); // byte_rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block_align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits_per_sample
    // data chunk
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for &s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

// ── response shapes (OpenAI-compatible) ──────────────────────────────────────

#[derive(Deserialize)]
struct TranscriptionResp {
    text: String,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_valid() {
        let pcm = vec![0i16; 16000]; // 1s silence @ 16kHz
        let wav = pcm_to_wav(&pcm, 16000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // data_len = 16000 * 2 = 32000
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 32000);
    }

    #[test]
    fn url_trims_trailing_slash() {
        assert_eq!(url("http://h:8000/", "/v1/"), "http://h:8000/v1/");
        assert_eq!(url("http://h:8000", "/v1/"), "http://h:8000/v1/");
    }
}
