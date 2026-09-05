//! dp-router — desk-pilot 本地 LLM 路由服务入口。
//!
//! 启动逻辑:
//!   1. 解析 CLI 参数(配置路径 / 监听地址 / llama-server 路径)
//!   2. 读 yaml 配置 → [`RouterConfig`](models = 模型目录,启动不预载)
//!   3. 启 axum 服务(默认 :8080)接收 OpenAI 兼容请求
//!      — 模型按需加载:首次请求某模型名才 spawn 对应 llama-server 子进程
//!   4. 后台 health-check 循环
//!
//! 用法:
//!   dp-router [--config dp-router.yaml] [--addr :8080] [--llama-server /path/to/llama-server]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::RwLock;
use tracing::{error, info};

mod config;
mod process;
mod router;
mod upstream;

use config::RouterConfig;
use process::ProcessMap;
use router::SharedState;

/// 默认配置文件查找顺序(由 `shared::loader!()` 的 prod/dev 规则决定 — 见 build.rs)。
const DEFAULT_CONF_PATH: &str = "dp-router.yaml";

#[derive(Parser, Debug)]
#[command(name = "dp-router", about = "desk-pilot 本地 LLM 路由(OpenAI 兼容)")]
struct Cli {
    /// 配置文件路径(dev 默认 dp-router.yaml,prod ~/.desk-pilot/dp-router.yaml)。
    #[arg(long)]
    config: Option<PathBuf>,
    /// dp-router 对外监听地址(覆盖 yaml 里的 server.addr)。
    #[arg(long)]
    addr: Option<String>,
    /// `llama-server` 二进制路径(覆盖 yaml 里的 llama_server.path)。
    #[arg(long)]
    llama_server: Option<PathBuf>,
}

fn main() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    shared::init_tracing();

    let cli = Cli::parse();
    let conf_path = cli.config.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_CONF_PATH));

    // 读 yaml 配置(从 `CONF::` 命名空间解析)。
    let fs = shared::loader!();
    let conf_path = match fs.resolve("CONF::").map(|p| p.join("dp-router.yaml")) {
        Some(p) if p.exists() => p,
        _ => conf_path,
    };
    let yaml = tokio::fs::read_to_string(&conf_path)
        .await
        .with_context(|| format!("read config {}", conf_path.display()))?;
    let mut cfg: RouterConfig = serde_yaml::from_str(&yaml)
        .with_context(|| format!("parse config {}", conf_path.display()))?;

    // CLI 覆盖
    if let Some(addr) = &cli.addr {
        cfg.server.addr = addr.clone();
    }
    if let Some(path) = &cli.llama_server {
        cfg.llama_server.path = path.clone();
    }

    // 归一化:所有路径字段(llama_server.path / models_root / gguf / mmproj)
    // 解析成绝对路径。必须在 CLI 覆盖之后、二进制存在性检查之前。
    cfg.resolve_paths()
        .with_context(|| format!("resolve paths in {}", conf_path.display()))?;

    // sanity check
    if !tokio::fs::try_exists(&cfg.llama_server.path).await.unwrap_or(false) {
        anyhow::bail!(
            "llama-server 二进制不存在: {} (请配置 llama_server.path 或传 --llama-server)",
            cfg.llama_server.path.display()
        );
    }

    info!(
        "[dp-router] 启动: addr={}, llama-server={}, 模型目录={} 个(按需加载,启动不预载), upstream={}",
        cfg.server.addr,
        cfg.llama_server.path.display(),
        cfg.models.len(),
        if cfg.remote_upstream.base_url.is_empty() { "关闭".to_string() } else { cfg.remote_upstream.base_url.clone() }
    );

    // 进程表
    let processes: ProcessMap = Arc::new(RwLock::new(Default::default()));

    // 上游客户端(若 base_url 空则 None)。
    // 转发超时:本地 LLM 长生成(CPU 上 2000 token ≈ 50-100s)会超过 60s,
    // 原 60s 会让 router 在 llama-server 仍在生成时放弃并 502。放宽到 300s
    // 支撑长生成;health_check 用独立的 5s 超时,不受影响。
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let upstream = upstream::UpstreamClient::new(cfg.remote_upstream.clone(), http.clone());

    let port_allocator = Arc::new(tokio::sync::Mutex::new(
        process::PortAllocator::new(cfg.llama_server.port_range),
    ));

    let state: SharedState = Arc::new(router::AppState {
        config: Arc::new(cfg),
        processes: processes.clone(),
        upstream,
        http,
        port_allocator,
        load_locks: Arc::new(tokio::sync::Mutex::new(Default::default())),
    });

    // 按需加载模式:启动不 spawn 任何子进程 — 配置 `models` 是"模型目录"(哪些模型
    // 可用),客户端首次请求某模型名时才拉起对应 llama-server(见 router::ensure_online)。

    // 启 health-check 后台循环
    router::spawn_health_loop(state.clone());

    // axum 服务
    let app = router::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(&state.config.server.addr)
        .await
        .with_context(|| format!("bind {}", state.config.server.addr))?;
    info!("[dp-router] listening on {}", state.config.server.addr);

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
        info!("[dp-router] 收到退出信号,graceful shutdown…");
    });
    if let Err(e) = serve.await {
        error!("[dp-router] serve error: {e}");
    }

    // 退出时清理子进程
    router::shutdown_all(&processes).await;
    info!("[dp-router] 已退出");
    Ok(())
}