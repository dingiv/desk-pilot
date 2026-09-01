//! aura-daemon — the audio-aura binary entry point: composition root(组装根)。
//! 流水线拼装(Stage1Config 组装/ASR·LLM 选型/模型加载/识别日志/段落归档)全部在
//! aura-core 的 [`Pipeline::assemble`] —— 这里只产出 [`PipelineSpec`]、按下开关、搭服务。
//!
//! 三层 server 结构(dependencies 单向向后):
//! - [`router`]   — axum 路由 + HTTP/SSE handler(参数提取 → service 调用 → 响应整形);
//! - [`service`]  — 业务状态与动作(DaemonState、双面协议、TurnEvent→线协议映射、idle 监控);
//! - [`repository`] — 持久化/数据访问(DataStore:Storage 构建 + 窄读接口);
//! - [`config`]   — 横切:CLI/yaml → Settings 解析(纯函数 + 单测)。
//!
//! main 只做接线:conf → tracing → 共享信号 → store → assemble → rt → serve。
//!
//! Threading: the pipeline runs as a常驻 tokio task on the socket runtime(round14 零专用
//! 线程;阻塞桥走 blocking pool)。识别 [`AsrEvent`]s 推 broadcast(数据面
//! /api/asr_stream);settings 变化 bump `version`(控制面,/api/stream ping `state_changed`,
//! clients re-GET /api/state)。Recognition events do NOT bump `version`.
//!
//! Run: cargo run -p aura-daemon -- 127.0.0.1:7879
//! Config precedence: CLI (high-frequency knobs, see `Cli`) > `aura.yaml` (full surface, dev:
//! this crate's dir, prod: ~/.desk-pilot/) > built-in defaults. No env vars — except `RUST_LOG`,
//! which overrides the `log_level` setting as the standard tracing escape hatch.

mod config;
mod repository;
mod router;
mod service;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::sync::{broadcast, Notify};
use tracing::{info, warn};

use audio_aura_agent::{AddHotwordTool, AsrEvent, HotwordManager, SharedHotwordManager};
use audio_aura_core::Pipeline;

use config::{Cli, ConfOrigin, Settings, AuraConf, config_view, resolve};
use repository::DataStore;
use router::serve_socket;
use service::{DaemonState, spawn_idle_monitor, turn_to_wire};

fn main() -> Result<()> {
    // Config loads FIRST — tracing init needs the configured `log_level`. The load runs
    // pre-subscriber, so it returns its origin instead of logging; we report it right after
    // the subscriber is up (nothing is lost, just emitted post-init).
    let (conf, origin) = AuraConf::load();
    let s = resolve(Cli::parse(), conf);
    // Init-stage side effect: the process-wide tracing subscriber (dev: human-readable;
    // release: JSON lines). Filter precedence: RUST_LOG env (escape hatch) >
    // aura.yaml `log_level` > "info". `effective_level` reports what actually took effect
    // (differs from the configured value on RUST_LOG override or invalid-value fallback).
    let effective_level = shared::init_tracing_with_filter(&s.log_level);
    match &origin {
        ConfOrigin::Yaml => info!(log_level = %effective_level, "conf loaded (aura.yaml)"),
        ConfOrigin::Json => info!(log_level = %effective_level, "conf loaded (aura.json)"),
        ConfOrigin::Defaults => {
            info!(log_level = %effective_level, "no aura.yaml / aura.json — built-in defaults")
        }
        ConfOrigin::Malformed(what) => {
            warn!(what, log_level = %effective_level, "conf parse error — fallback in effect")
        }
    }
    let Settings { bind_addr, port, stage3_on, web_dist, recordings_dir, recordings_retention_days, log_level, spec, idle_timeout } = s;

    // Connection toggle + shared snapshot state, shared across the Pipeline task + socket
    // handlers. (No event bus — SSE pings off the `version` counter; data lives in the snapshot.)
    let active = Arc::new(AtomicBool::new(true));
    // idle 深度睡眠信号: false → Stage1 消费循环退出 + 断开 scout; 恢复时置回 true。
    let running = Arc::new(AtomicBool::new(true));
    // 主动归档信号(IME 分字符 `'` = "我说完了"):socket 置 true → Stage1 消费循环
    // 跳过 merge_gap 剩余等待,立即整段 batch。识别域动作,不 bump version ——
    // 结果经数据面 /api/asr_stream 推送。
    let flush_paragraph = Arc::new(AtomicBool::new(false));
    // idle 恢复唤醒: daemon 在下一个客户端连接时置 running=true + notify pipeline 线程。
    let resume_cv: Arc<Notify> = Arc::new(Notify::new());
    let version = Arc::new(AtomicU64::new(0));
    // Data-plane channel: recognition sentences pushed to /api/asr_stream subscribers.
    let (asr_events, _) = broadcast::channel::<AsrEvent>(1024);

    // Shared hotword store = the Stage3→Stage2 feedback channel (seeded from the config /
    // built-in list; Stage3 grows it at runtime).
    let hotwords: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(spec.hotwords.clone()));
    let corrections: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mgr: Arc<dyn HotwordManager> = Arc::new(SharedHotwordManager::new(Arc::clone(&hotwords)));
    let tool = AddHotwordTool::new(Arc::clone(&mgr));

    // ── 持久化层:Storage 监督器(recordings WAV + turns day log + recent ring)──
    let store = DataStore::build(recordings_dir, recordings_retention_days);

    // ── 全栈拼装在 core(Stage1Config 组装/ASR·LLM 选型/模型加载/预热/识别日志/段落归档)──
    // TODO: 这里是核心的模型推理触发点——assemble 加载模型,spawn 启动推理循环。
    let pipeline = Pipeline::assemble(
        &spec,
        Arc::clone(&active),
        Arc::clone(&running),
        Arc::clone(&flush_paragraph),
        Arc::clone(&hotwords),
        Arc::clone(&corrections),
        Some(store.storage()), // ParagraphCalibration 时自动 record_final(archive+day log+ring)
    )?;

    // ── Pipeline on the socket runtime(round14:零专用线程)── 识别事件 → 数据面;
    //    Stage3 挂段落定稿。无事件总线:SSE handler 靠 `version` ping。
    //    runtime 提前建好,pipeline.run 作为常驻任务 spawn 上去(阻塞桥走 blocking pool)。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("aura-socket")
        .build()?;
    {
        let tool = tool.clone();
        let version = Arc::clone(&version);
        let asr_events = asr_events.clone();
        rt.spawn(pipeline.run(Arc::clone(&running), Arc::clone(&resume_cv), move |ev| {
            // Recognition events → DATA plane only (broadcast the sentence). The control
            // plane (version/snapshot) is NOT bumped here — only settings changes bump it.
            // (识别日志与段落归档在 core 的 run() 内部——这里只做线协议映射。)
            if let Some(seg) = turn_to_wire(stage3_on, &tool, &version, ev) {
                let _ = asr_events.send(seg);
            }
        }));
    }

    // ── Socket on the same runtime (main thread block_on) ──
    let state = DaemonState {
        hotwords: Arc::clone(&hotwords),
        corrections: Arc::clone(&corrections),
        active: Arc::clone(&active),
        running: Arc::clone(&running),
        flush_paragraph: Arc::clone(&flush_paragraph),
        idle: Arc::new(AtomicBool::new(false)),
        subscribers: Arc::new(AtomicUsize::new(0)),
        resume_notify: Arc::clone(&resume_cv),
        idle_timeout: idle_timeout.map(Duration::from_secs),
        version: Arc::clone(&version),
        asr_events: asr_events.clone(),
        config: config_view(&spec),
        stage3_on,
        store,
    };
    // idle 深度睡眠监控:无 SSE 订阅持续 idle_timeout → 进入 idle(Stage1 退出 + 断开 scout)。
    spawn_idle_monitor(&rt, state.clone());
    info!(port, "socket: http://{bind_addr}:{port}  (/api/state /api/stream /api/control/scout /api/correct /api/audio)");
    info!(scout = %spec.scout_addr, stage3 = stage3_on, log_level = %log_level, idle_timeout = ?idle_timeout, "pipeline running as rt task(round14 零专用线程)— Ctrl-C 结束");
    rt.block_on(serve_socket(state, bind_addr, port, web_dist));
    Ok(())
}
