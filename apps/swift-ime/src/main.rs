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

mod backends;
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
            backends::ibus::IbusAdapter::new();
            std::thread::park();
        }

        "mock" => {
            info!("mock backend — reading stdin, type /greet or /sig to test");
            run_mock(&cli.config)?;
        }

        other => {
            anyhow::bail!("unknown backend: {other} (expected fcitx5, ibus, or mock)");
        }
    }

    Ok(())
}

fn run_mock(config_path: &str) -> Result<()> {
    use ime_core::{Dispatcher, Expander, ImeState, Matcher, SnippetStore, platform::NoopPinyin};
    use ime_core::expander::StaticProvider;
    use std::io::{self, Write};

    let store = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| SnippetStore::from_json(&s).ok())
        .unwrap_or_else(|| {
            SnippetStore::from_json(DEFAULT_SNIPPETS).unwrap_or_else(|_| SnippetStore::new())
        });

    let matcher = Matcher::new(store.entries());
    let expander = Expander::new(Box::new(StaticProvider {
        date: "2026-07-23".into(),
        clipboard: String::new(),
    }));
    let dispatcher = Dispatcher::new(matcher, expander, Box::new(NoopPinyin));
    println!("swift-ime mock — type a line and press Enter. Trigger prefixes: / and #");
    println!("Type /greet or /sig to test. Ctrl-C to exit.\n");

    let mut input = String::new();
    loop {
        input.clear();
        print!("> ");
        io::stdout().flush()?;
        if io::stdin().read_line(&mut input)? == 0 {
            break;
        }
        let mut state = ImeState::default();
        for ch in input.trim_end().chars() {
            match dispatcher.process_key(ch, &mut state) {
                ime_core::ImeAction::PassThrough => {}
                ime_core::ImeAction::Preedit { text, .. } => {
                    print!("[{text}]");
                    io::stdout().flush()?;
                }
                ime_core::ImeAction::Commit(text) => {
                    println!(" → {text}");
                }
                _ => {}
            }
        }
        if !state.buffer.is_empty() {
            println!(" → {}", state.buffer);
        }
    }
    Ok(())
}

const DEFAULT_SNIPPETS: &str = r##"[
    {"trigger": "/greet", "expand": "你好，我是 AI 秘书，请问有什么可以帮你的？", "desc": "通用问候语"},
    {"trigger": "/sig", "expand": "Best regards,\nAlice\n$DATE", "desc": "邮件签名"}
]"##;
