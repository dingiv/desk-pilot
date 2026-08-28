//! swift-ime — DeskPilot IME engine.
//!
//! This binary is the **debug server**: one engine serving both the TUI
//! (keyboard) and a Unix socket (`swift_cli` sends keys from the command
//! line, getting back the JSON prediction view — multi-line core debugging).
//!
//! ```bash
//! cargo run --bin swift-ime                    # TUI + socket (default /tmp/swift-ime.sock)
//! cargo run --bin swift-ime -- --no-tui        # headless: socket only (no tty needed)
//! cargo run --bin swift-ime -- --input nihao --verbose   # single query (eval tool)
//! cargo run --bin swift-ime -- --cases test.txt         # batch eval
//! ```
//!
//! Companion CLI: `cargo run --bin swift_cli -- ti'an space`

use clap::Parser;
use swift_ime::frontends::tui::{self, TuiConfig};

#[derive(Parser)]
#[command(name = "swift-ime", about = "DeskPilot IME debug server")]
struct Args {
    #[arg(long)]
    cases: Option<String>,
    #[arg(long, default_value = "160")]
    top_n: usize,
    #[arg(long, default_value = "true")]
    verbose: bool,
    #[arg(long)]
    input: Option<String>,
    #[arg(long)]
    config: Option<String>,
    #[arg(long)]
    asr_text: Option<String>,
    #[arg(long, default_value = "false")]
    commit: bool,
    #[arg(long, default_value = "0")]
    async_wait: u64,
    /// aura daemon origin(默认 http://127.0.0.1:9091)。voice listener 在引擎构造时启动,
    /// 跟随引擎 drop 自动清理 —— 这里只是配置 URL。
    #[arg(long)]
    aura_addr: Option<String>,
    #[arg(long)]
    en_user_dict: Option<String>,
    #[arg(long)]
    en_dicts: Vec<String>,
    /// `#req` backend base URL (default http://127.0.0.1:14555/api).
    #[arg(long)]
    req_base: Option<String>,
    /// Unix socket 路径(默认 /tmp/swift-ime.sock;swift_cli 用同路径连接)。
    #[arg(long)]
    sock: Option<String>,
    /// 无 TUI 纯 socket 模式(无 tty 环境 / 远程调试;Ctrl-C 结束)。
    #[arg(long, default_value = "false")]
    no_tui: bool,
}

fn main() {
    let args = Args::parse();
    let cfg = TuiConfig {
        cases: args.cases.clone(), input: args.input.clone(),
        top_n: args.top_n, verbose: args.verbose,
        config: args.config, asr_text: args.asr_text,
        commit: args.commit, async_wait: args.async_wait,
        voice_aura_base: args.aura_addr
            .unwrap_or_else(|| ime_core::engine::DEFAULT_VOICE_AURA_BASE.to_string()),
        voice_idle_time: ime_core::io_thread::DEFAULT_IDLE_TIMEOUT_SECS,
        en_user_dict: args.en_user_dict, en_dicts: args.en_dicts,
        req_base: args.req_base,
        frontend: None,
        tee_stderr: true,
    };

    if let Some(ref cases_path) = cfg.cases {
        tui::run_cases_mode(&cfg, cases_path);
    } else if cfg.input.is_some() {
        tui::run_input_mode(&cfg);
    } else {
        tui::run_server(cfg, args.sock, args.no_tui).expect("debug server crashed");
    }
}
