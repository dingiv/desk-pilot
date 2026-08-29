//! LlamaProcess — 管理一个 `llama-server` 子进程的生命周期。
//!
//! 每个本地模型一个子进程,独占一个内部 HTTP 端口(在 `LlamaServerConfig.port_range`
//! 内分配)。dp-router 通过 reqwest 转发 OpenAI 兼容请求到这些端口。

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::config::{
    resolve_gguf_path, LocalModelConfig, LlamaServerConfig, ModelRuntimeStatus, ModelType,
};

/// 子进程包装。持有 Child handle + 端口 + 重启计数。
pub struct LlamaProcess {
    pub model_name: String,
    pub port: u16,
    pub config: LocalModelConfig,
    child: Option<Child>,
    /// 实际 PID(便于诊断)。
    pub pid: Option<u32>,
    /// 累计重启次数(成功 spawn 后清零)。
    pub restarts: u32,
    pub status: ModelRuntimeStatus,
    http: reqwest::Client,
}

impl LlamaProcess {
    pub fn new(config: LocalModelConfig, port: u16, http: reqwest::Client) -> Self {
        Self {
            model_name: config.name.clone(),
            port,
            config,
            child: None,
            pid: None,
            restarts: 0,
            status: ModelRuntimeStatus::Starting,
            http,
        }
    }

    /// 拼出 spawn 用的命令行。`port` = 已分配的子进程端口;`gguf` = 已解析的绝对路径。
    pub fn build_command(
        llama_path: &Path,
        gguf: &Path,
        cfg: &LocalModelConfig,
        port: u16,
    ) -> Command {
        let mut cmd = Command::new(llama_path);
        cmd.arg("-m").arg(gguf)
            .arg("--port").arg(port.to_string())
            .arg("-c").arg(cfg.context_size.to_string())
            .arg("--threads").arg(cfg.threads.to_string())
            .arg("-b").arg(cfg.batch_size.to_string());
        if cfg.gpu_layers > 0 {
            cmd.arg("-ngl").arg(cfg.gpu_layers.to_string());
        }
        // ASR 模型:附 mmproj 多模态投影器,llama-server 加载后自动启用
        // /v1/audio/transcriptions。解析失败仅告警(子进程会以纯 LLM 起来,请求时报错)。
        if cfg.r#type == ModelType::Asr {
            match cfg.mmproj.as_deref().map(resolve_gguf_path) {
                Some(Ok(p)) => {
                    cmd.arg("--mmproj").arg(&p);
                }
                Some(Err(e)) => {
                    warn!(
                        "[dp-router] mmproj resolve failed (model={}): {e} — spawning without it",
                        cfg.name
                    );
                }
                None => {
                    warn!(
                        "[dp-router] type=asr but mmproj unset (model={}) — \
                         audio endpoint will 400",
                        cfg.name
                    );
                }
            }
        }
        for a in &cfg.extra_args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// 首次 spawn(或失败后由 [`Self::restart`] 重试)。返回成功后的 PID。
    pub async fn spawn(&mut self, llama_path: &Path, gguf: &Path) -> Result<u32> {
        let mut cmd = Self::build_command(llama_path, gguf, &self.config, self.port);
        info!(
            "[dp-router] spawning llama-server: model={} port={} gguf={}",
            self.model_name, self.port, gguf.display()
        );
        let child = cmd.spawn().with_context(|| {
            format!("spawn llama-server failed (model={}, port={})", self.model_name, self.port)
        })?;
        let pid = child.id().unwrap_or(0);
        self.pid = Some(pid);
        self.child = Some(child);
        self.status = ModelRuntimeStatus::Starting;
        Ok(pid)
    }

    /// `/health` 探活 — llama-server 的 health endpoint 是 `/health`。
    pub async fn health_check(&self) -> bool {
        let url = format!("http://127.0.0.1:{}/health", self.port);
        match self.http.get(&url).timeout(std::time::Duration::from_secs(2)).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    /// 等到 `/health` 通(最长 timeout_secs 秒)。返回是否就绪。
    pub async fn wait_ready(&mut self, timeout_secs: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        while std::time::Instant::now() < deadline {
            if self.health_check().await {
                self.status = ModelRuntimeStatus::Online;
                return true;
            }
            // 子进程是否已退出?
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    error!(
                        "[dp-router] llama-server exited prematurely: model={} status={:?}",
                        self.model_name, status
                    );
                    self.status = ModelRuntimeStatus::Offline;
                    return false;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        warn!(
            "[dp-router] llama-server not ready after {timeout_secs}s: model={} port={}",
            self.model_name, self.port
        );
        self.status = ModelRuntimeStatus::Offline;
        false
    }

    /// 重启子进程(指数退避)。超过 `restart_max_retries` 放弃。
    pub async fn restart(
        &mut self,
        llama_path: &Path,
        gguf: &Path,
        ll_cfg: &LlamaServerConfig,
    ) -> Result<bool> {
        if self.restarts >= ll_cfg.restart_max_retries {
            error!(
                "[dp-router] restart budget exhausted: model={} restarts={}",
                self.model_name, self.restarts
            );
            self.status = ModelRuntimeStatus::Offline;
            return Ok(false);
        }
        self.restarts += 1;
        let delay = ll_cfg.restart_backoff_base_s * 2u64.pow(self.restarts.min(6));
        warn!(
            "[dp-router] restarting llama-server: model={} attempt={} delay={}s",
            self.model_name, self.restarts, delay
        );
        // 杀旧 child
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        self.spawn(llama_path, gguf).await?;
        // 等待就绪(给 30s,单次 spawn 后模型加载时间通常 < 30s)
        Ok(self.wait_ready(30).await)
    }

    /// 优雅关闭。
    pub async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.status = ModelRuntimeStatus::Offline;
    }
}

/// 端口分配器(线性扫描 `port_range`,已被占用的跳过 — `bind` 失败时回收)。
pub struct PortAllocator {
    next: u16,
    end: u16,
}

impl PortAllocator {
    pub fn new(range: [u16; 2]) -> Self {
        let [from, to] = range;
        Self { next: from, end: to }
    }
    pub fn next(&mut self) -> Option<u16> {
        let p = self.next;
        if p > self.end {
            return None;
        }
        self.next = self.next.checked_add(1).unwrap_or(self.end + 1);
        Some(p)
    }
}

/// 共享进程表:name → LlamaProcess 的 Arc<RwLock<...>>。
pub type ProcessMap = Arc<RwLock<std::collections::HashMap<String, Arc<RwLock<LlamaProcess>>>>>;
