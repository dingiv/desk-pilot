//! LlamaProcess — 管理一个 `llama-server` 子进程的生命周期。
//!
//! 每个本地模型一个子进程,独占一个内部 HTTP 端口(在 `LlamaServerConfig.port_range`
//! 内分配)。dp-router 通过 reqwest 转发 OpenAI 兼容请求到这些端口。

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::{
    resolve_gguf_path, LocalModelConfig, LlamaServerConfig, ModelRuntimeStatus, ModelType,
};

/// 健康检查超时。llama-server 生成/转写繁忙时 /health 可能被延迟,2s 在高负载机器上
/// 会造成误判(误判 → router SIGKILL 健康子进程,见 2026-08-30 事故)。
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// 连续在线健康满 60s → 清零重启预算(崩溃循环计数只限"连续",不限制一生)。
const BUDGET_RESET_ONLINE: Duration = Duration::from_secs(60);
/// 重启预算耗尽后的冷却时间;冷却结束清零预算,允许下一轮尝试(不再永久 offline)。
const RESTART_COOLDOWN: Duration = Duration::from_secs(60);

/// 子进程包装。持有 Child handle + 端口 + 重启计数。
pub struct LlamaProcess {
    pub model_name: String,
    pub port: u16,
    pub config: LocalModelConfig,
    child: Option<Child>,
    /// 实际 PID(便于诊断)。
    pub pid: Option<u32>,
    /// 连续重启次数(连续在线 60s 后清零;与 [`RESTART_COOLDOWN`] 配合限制崩溃循环)。
    pub restarts: u32,
    pub status: ModelRuntimeStatus,
    /// 连续健康检查失败计数(单次失败不触发重启,防瞬时误判杀健康进程)。
    pub failed_checks: u32,
    /// 当前 child 已确认退出(reap 过)。tokio Child 被 reap 后 try_wait 不再可查,需自行记忆。
    child_exited: bool,
    /// 转为 Online 的时刻(重启预算恢复用)。
    pub online_since: Option<Instant>,
    /// 最近一次重启尝试时刻(冷却期判断用)。
    last_restart: Option<Instant>,
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
            failed_checks: 0,
            child_exited: false,
            online_since: None,
            last_restart: None,
            http,
        }
    }

    /// 拼出 spawn 用的命令行。`port` = 已分配的子进程端口;`gguf` = 已解析的绝对路径。
    pub fn build_command(
        llama_path: &Path,
        gguf: &Path,
        cfg: &LocalModelConfig,
        port: u16,
        env: &HashMap<String, String>,
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
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// 首次 spawn(或失败后由 [`Self::restart`] 重试)。返回成功后的 PID。
    pub async fn spawn(
        &mut self,
        llama_path: &Path,
        gguf: &Path,
        env: &HashMap<String, String>,
    ) -> Result<u32> {
        let mut cmd = Self::build_command(llama_path, gguf, &self.config, self.port, env);
        info!(
            "[dp-router] spawning llama-server: model={} port={} gguf={}",
            self.model_name, self.port, gguf.display()
        );
        let mut child = cmd.spawn().with_context(|| {
            format!("spawn llama-server failed (model={}, port={})", self.model_name, self.port)
        })?;
        let pid = child.id().unwrap_or(0);
        // 接管子进程 stdout/stderr:逐行转投 tracing。一方面消除管道写满阻塞
        // (Linux pipe 缓冲仅 64KB,llama-server 模型加载日志量轻易超过,不读会
        // 死锁子进程);另一方面把 n_layer / mmproj / 错误等关键行带进统一日志。
        if let Some(out) = child.stdout.take() {
            pipe_to_tracing(out, self.model_name.clone(), self.port, self.config.gpu_layers);
        }
        if let Some(err) = child.stderr.take() {
            pipe_to_tracing(err, self.model_name.clone(), self.port, self.config.gpu_layers);
        }
        self.pid = Some(pid);
        self.child = Some(child);
        self.status = ModelRuntimeStatus::Starting;
        self.child_exited = false;
        self.failed_checks = 0;
        self.online_since = None;
        Ok(pid)
    }

    /// 探测子进程是否仍存活;已退出则 reap 并记忆。
    ///
    /// tokio `Child` 被 reap 后 `try_wait` 不再返回退出状态,所以结果记在
    /// `child_exited` 里供后续轮次使用(否则"已死的进程"会被误判为"活着")。
    pub fn check_alive(&mut self) -> bool {
        if self.child_exited {
            return false;
        }
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.child_exited = true;
                error!(
                    "[dp-router] llama-server exited: model={} status={:?}",
                    self.model_name, status
                );
                false
            }
            _ => true,
        }
    }

    /// `/health` 探活 — llama-server 的 health endpoint 是 `/health`。
    pub async fn health_check(&self) -> bool {
        let url = format!("http://127.0.0.1:{}/health", self.port);
        match self.http.get(&url).timeout(HEALTH_TIMEOUT).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    /// 连续在线满 60s → 清零重启预算(崩溃循环只限"连续"重启,不惩罚长期健康)。
    pub fn budget_recovery(&mut self) {
        if self.restarts > 0
            && self
                .online_since
                .is_some_and(|t| t.elapsed() >= BUDGET_RESET_ONLINE)
        {
            self.restarts = 0;
        }
    }

    /// 重启子进程(指数退避)。
    ///
    /// 预算语义:`restarts` 连续满 `restart_max_retries` 且进程没撑过 60s 健康期 →
    /// 进入 [`RESTART_COOLDOWN`] 冷却;冷却结束清零预算,再来一轮。不再有"永久
    /// offline"——健康进程被误判后能自行恢复(见 `run_once` 的自愈分支)。
    pub async fn restart(
        &mut self,
        llama_path: &Path,
        gguf: &Path,
        ll_cfg: &LlamaServerConfig,
    ) -> Result<()> {
        let now = Instant::now();
        if self.restarts >= ll_cfg.restart_max_retries {
            if self
                .last_restart
                .is_some_and(|t| now.duration_since(t) < RESTART_COOLDOWN)
            {
                warn!(
                    "[dp-router] restart budget exhausted, cooling down: model={} restarts={}",
                    self.model_name, self.restarts
                );
                self.status = ModelRuntimeStatus::Offline;
                return Ok(());
            }
            info!(
                "[dp-router] cooldown over, resetting restart budget: model={}",
                self.model_name
            );
            self.restarts = 0;
        }
        self.restarts += 1;
        self.last_restart = Some(now);
        let delay = ll_cfg.restart_backoff_base_s * 2u64.pow(self.restarts.min(6));
        warn!(
            "[dp-router] restarting llama-server: model={} attempt={} delay={}s",
            self.model_name, self.restarts, delay
        );
        // 杀旧 child(SIGKILL — 进程无输出即"消失",诊断靠 check_alive 的退出状态)
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.child_exited = true;
        tokio::time::sleep(Duration::from_secs(delay)).await;
        self.spawn(llama_path, gguf, &ll_cfg.env).await?;
        // 不阻塞等就绪:spawn 已置 status=Starting,健康循环负责 Starting→Online
        // (数据面请求自己轮询子进程 /health,见 router::ensure_online)。
        Ok(())
    }

    /// 等子进程就绪:轮询 `/health` 直到成功(置 Online)或超时。
    /// 加载期间子进程死亡 → 立即返回 false(交给健康循环带预算重启)。
    ///
    /// 每次探活短暂持写锁,探活间隙(500ms sleep)释放锁 — 避免慢加载时长时间
    /// 独占进程锁,阻塞健康循环探活 / 数据面 `wait_online` 读状态 / admin 查询。
    pub async fn wait_ready(lock: &RwLock<LlamaProcess>, timeout_s: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(timeout_s);
        loop {
            let mut p = lock.write().await;
            if p.health_check().await {
                p.status = ModelRuntimeStatus::Online;
                p.online_since = Some(Instant::now());
                p.failed_checks = 0;
                return true;
            }
            if !p.check_alive() {
                p.status = ModelRuntimeStatus::Offline;
                return false;
            }
            // 探活间隙释放锁,再 sleep
            drop(p);
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// 优雅关闭。
    pub async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        self.child_exited = true;
        self.status = ModelRuntimeStatus::Offline;
    }
}

/// 子进程 stdout/stderr 流 → tracing 逐行转发(每个 spawn 起两个 task,管道 EOF 即退出)。
///
/// 关键行提升(其余 debug,默认 RUST_LOG=info 不可见):
///   - `n_layer = N`       → info(总层数;对照 gpu_layers 即知 GPU 卸载余量)
///   - 含 `mmproj`          → info(多模态投影器加载结果;没加载成功 ASR 不可用)
///   - 含 error / fail      → warn
fn pipe_to_tracing(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    model: String,
    port: u16,
    gpu_layers: u32,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if let Some(n) = parse_n_layer(line) {
                let offload = if gpu_layers == 0 {
                    "cpu".into()
                } else if gpu_layers >= n {
                    format!("all {n} layers on gpu")
                } else {
                    format!("{gpu_layers}/{n} layers on gpu")
                };
                info!(model = %model, port, n_layer = n, offload = %offload, "[llama-server] {line}");
            } else if line.to_lowercase().contains("mmproj") {
                info!(model = %model, port, "[llama-server] {line}");
            } else {
                let low = line.to_lowercase();
                if low.contains("error") || low.contains("fail") {
                    warn!(model = %model, port, "[llama-server] {line}");
                } else {
                    debug!(model = %model, port, "[llama-server] {line}");
                }
            }
        }
    });
}

/// 从 llama.cpp 日志行解析总层数 `n_layer = N`。
/// `n_layer_kv_cache = 0` 这类(n_layer 后面不是紧跟 `=`)返回 None。
fn parse_n_layer(line: &str) -> Option<u32> {
    let idx = line.find("n_layer")?;
    let rest = line[idx + "n_layer".len()..].trim_start().strip_prefix('=')?;
    rest.trim().split_whitespace().next()?.parse().ok()
}

/// 端口分配器(从 `port_range` 起点单调递增发号,耗尽返回 None)。
///
/// 注意:不做"端口是否已被占用"的预检,也不回收(无卸载端点,端口只发不回收)。
/// 若某端口被外部进程占用,子进程 `bind` 失败 → 退出 → 健康循环带预算重启兜底。
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

#[cfg(test)]
mod tests {
    use super::parse_n_layer;

    #[test]
    fn parse_n_layer_extracts_total() {
        // llama.cpp 实际打印格式(llm_load_print_meta)
        assert_eq!(parse_n_layer("llm_load_print_meta: n_layer = 36"), Some(36));
        assert_eq!(parse_n_layer("llm_load_print_meta: n_layer = 28"), Some(28));
        // 紧凑写法也容得下
        assert_eq!(parse_n_layer("n_layer=12"), Some(12));
        // 非总层数键 / 普通行 → None
        assert_eq!(parse_n_layer("n_layer_kv_cache = 0"), None);
        assert_eq!(parse_n_layer("n_gpu_layers = 99"), None);
        assert_eq!(parse_n_layer("server is listening"), None);
    }
}
