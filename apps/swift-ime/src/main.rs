//! swift-ime — DeskPilot input method engine.
//!
//! Two deployment modes:
//! - **fcitx5 addon (.so)**: loaded by fcitx5, C++ thin glue calls our `extern "C"` API.
//!   The `.so` is built via CMake + cargo-c (the binary target is not used in this mode).
//! - **Standalone (ibus / debug)**: this binary runs as a standalone process. For ibus,
//!   it registers as a DBus engine. In `--backend mock` mode it just exercises ime-core
//!   against stdin (for dev/test without an IME framework).
//!
//! ```bash
//! # Mock mode (dev/test):
//! cargo run -p swift-ime -- --backend mock
//!
//! # ibus mode (Phase 4):
//! cargo run -p swift-ime -- --backend ibus
//! ```

mod frontends;
mod bridge;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "swift-ime", about = "DeskPilot input method engine")]
struct Cli {
    /// Platform backend: "fcitx5" (loaded as .so), "ibus" (DBus engine), "mock" (stdin test)
    #[arg(long, default_value = "mock")]
    backend: String,

    /// Path to snippet config JSON (default: ime.json in current dir)
    #[arg(long, default_value = "ime.json")]
    config: String,
}

fn main() -> Result<()> {
    shared::init_tracing();
    let cli = Cli::parse();

    info!(backend = %cli.backend, config = %cli.config, "swift-ime starting");

    match cli.backend.as_str() {
        "fcitx5" => {
            let config = std::fs::read_to_string(&cli.config)
                .unwrap_or_else(|_| String::from("[]"));
            info!(snippets = config.len(), "fcitx5 mode — .so loaded by fcitx5");
            std::thread::park();
        }

        "ibus" => {
            info!("ibus backend — stub (Phase 4)");
            frontends::ibus::IbusAdapter::new();
            std::thread::park();
        }

        "mock" => {
            info!("mock backend — reading stdin, type /greet or #date to test");
            run_mock(&cli.config)?;
        }

        other => {
            anyhow::bail!("unknown backend: {other} (expected fcitx5, ibus, or mock)");
        }
    }

    Ok(())
}

fn run_mock(_config_path: &str) -> Result<()> {
    use ime_core::engine::{ImeEngine, InputEvent};
    use ime_core::ImeView;
    use std::io::{self, Write};

    let mut engine = ImeEngine::new();
    println!("swift-ime mock — type a line and press Enter. Trigger prefixes: / and #");
    println!("Type /greet, #date, or pinyin (ni, nihao) to test. Ctrl-C to exit.\n");

    let mut input = String::new();
    loop {
        input.clear();
        print!("> ");
        io::stdout().flush()?;
        if io::stdin().read_line(&mut input)? == 0 {
            break Ok(());
        }
        for ch in input.trim_end_matches(['\n', '\r']).chars() {
            let view = engine.predict(InputEvent::char(ch));

            if view.key_passthrough != 0 {
                continue;
            }

            let commit = ImeView::str_field(&view.commit_text);
            if !commit.is_empty() {
                println!(" → {commit}");
                continue;
            }

            let preedit = ImeView::str_field(&view.preedit_text);
            if view.candidate_count > 0 {
                let mut cands = String::new();
                for i in 0..view.candidate_count as usize {
                    if i > 0 { cands.push('/'); }
                    cands.push_str(ImeView::str_field(&view.candidates[i].text));
                }
                print!("[{preedit} → {cands}]");
                io::stdout().flush()?;
            } else if !preedit.is_empty() {
                print!("[{preedit}]");
                io::stdout().flush()?;
            }
        }
        // Commit any remaining buffer (emulates Enter at end of line).
        let tail = engine.buffer();
        if !tail.is_empty() {
            println!(" → {tail}");
            engine.predict(InputEvent::enter());
        } else {
            println!();
        }
    }
}
