//! swift-ime — DeskPilot IME engine.
//!
//! This binary is the **Mock frontend** evaluation tool.
//! Without `--input` or `--cases`, enters interactive TUI mode.
//!
//! ```bash
//! cargo run --bin swift-ime                           # TUI mode
//! cargo run --bin swift-ime -- --input nihao --verbose # single query
//! cargo run --bin swift-ime -- --cases test.txt        # batch eval
//! ```

mod tui;

use clap::Parser;
use swift_ime::frontends::mock::{self, MockConfig};

#[derive(Parser)]
#[command(name = "swift-ime", about = "DeskPilot IME evaluation tool")]
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
    #[arg(long, default_value = "false")]
    aura: bool,
    #[arg(long)]
    aura_addr: Option<String>,
    #[arg(long)]
    surrounding: Option<String>,
    #[arg(long)]
    en_user_dict: Option<String>,
    #[arg(long)]
    en_dicts: Vec<String>,
    /// `#req` backend base URL (default http://127.0.0.1:14555/api).
    #[arg(long)]
    req_base: Option<String>,
}

fn main() {
    let args = Args::parse();
    let cfg = MockConfig {
        cases: args.cases.clone(), input: args.input.clone(),
        top_n: args.top_n, verbose: args.verbose,
        config: args.config, asr_text: args.asr_text,
        commit: args.commit, async_wait: args.async_wait,
        connect_aura: args.aura, aura_addr: args.aura_addr,
        surrounding: args.surrounding,
        en_user_dict: args.en_user_dict, en_dicts: args.en_dicts,
        req_base: args.req_base,
    };

    if let Some(ref cases_path) = cfg.cases {
        mock::run_cases_mode(&cfg, cases_path);
    } else if cfg.input.is_some() {
        mock::run_input_mode(&cfg);
    } else {
        tui::run(cfg).expect("TUI crashed");
    }
}
