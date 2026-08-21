//! Unified logging for swift-ime and ime-core — one `tracing` subscriber.
//!
//! Both crates log through the `tracing` facade:
//! - ime-core uses `tracing::{error, warn, info, debug}!` directly;
//! - swift-ime keeps [`ime_log!`] as a thin alias to `tracing::info!`
//!   (target `swift_ime`) so existing call sites route into the same subscriber.
//!
//! A single process-wide subscriber (installed ONCE at frontend init) writes to
//! a log file — the **only reliable channel** when the `.so` runs inside the
//! fcitx5 daemon, which detaches stdout/stderr:
//! - **Dev** (`debug_assertions`): human-readable lines, teed to stderr + file.
//! - **Release**: one JSON object per line to the file (machine-parseable).
//! - **Level filter**: `RUST_LOG` env overrides the config default (`"info"`).
//!   Accepts a bare level or per-target directives (`ime_core=debug,info`).
//!
//! File location: `DATA::swift-ime.log` — dev `apps/swift-ime/data/`, prod
//! `~/.desk-pilot/`. Truncated on open if larger than [`MAX_BYTES`].

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, Once, OnceLock};

use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::EnvFilter;

/// Log file size cap on open (bytes).
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// The shared append-mode log file. Held for the process lifetime; each tracing
/// event briefly locks it to write one line.
static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// `MakeWriter` for tracing-subscriber: appends one event line to the log file.
/// No-op when the logger isn't initialized (tests / no frontend).
#[derive(Debug)]
struct FileWriter;

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = FileGuard;
    fn make_writer(&'a self) -> Self::Writer {
        FileGuard
    }
}

#[derive(Debug)]
struct FileGuard;

impl Write for FileGuard {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(f) = LOG_FILE.get() {
            f.lock().unwrap().write(buf)
        } else {
            Ok(buf.len()) // swallow — logger not initialized
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = LOG_FILE.get() {
            f.lock().unwrap().flush()
        } else {
            Ok(())
        }
    }
}

/// Open (create/append, truncate if oversized) the log file.
fn open_log_file(path: &str) -> Option<Mutex<std::fs::File>> {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > MAX_BYTES {
            let _ = fs::write(path, "");
        }
    }
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
        .map(Mutex::new)
}

/// Install the process-wide `tracing` subscriber. Idempotent via [`Once`] ——
/// call at frontend init (fcitx5 `swift_ime_create` / mock / TUI).
///
/// - 每条事件写 `path`(文件 —— fcitx5 daemon 下 stderr 不连终端,文件是
///   唯一可靠信道);
/// - dev 下 `tee_stderr=true` 双写 stderr(人类可读),`false` 只写文件
///   (TUI 用 alternate screen,stderr 会打坏界面);release 只写 JSON 行到文件;
/// - 级别:`RUST_LOG` 环境变量 > `default_filter`(裸级别或 per-target 指令)。
pub fn init_tracing(path: &str, default_filter: &str, tee_stderr: bool) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if let Some(f) = open_log_file(path) {
            let _ = LOG_FILE.set(f);
        }
        let filter = std::env::var("RUST_LOG")
            .ok()
            .filter(|v| !v.is_empty())
            .and_then(|v| EnvFilter::try_new(v).ok())
            .unwrap_or_else(|| EnvFilter::new(default_filter));
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let registry = tracing_subscriber::registry().with(filter);
        if cfg!(debug_assertions) && tee_stderr {
            // 文件 + stderr 双写。std::io::stderr 是 `fn() -> Stderr`,
            // 满足 `MakeWriter` 的 `F: Fn() -> W, W: Write` 实现。
            registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_writer(tracing_subscriber::fmt::writer::Tee::new(
                            FileWriter,
                            std::io::stderr,
                        )),
                )
                .init();
        } else {
            // 文件 only(release 恒走这;dev 的 TUI 也走这,避免打坏 alternate screen)。
            registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_writer(FileWriter),
                )
                .init();
        }
    });
}

/// Init with default path (`DATA::swift-ime.log`) and level `"info"`。
pub fn init_default() {
    init_with_log_level(None, true);
}

/// Init with a config-provided level(`debug.log_level`,默认 `"info"`)。
/// `RUST_LOG` 环境变量仍然优先。`tee_stderr`: dev 下是否同时打到 stderr
/// (CLI/mock/fcitx 可;TUI 必须 `false`,否则日志破坏 alternate screen)。
pub fn init_with_log_level(level: Option<&str>, tee_stderr: bool) {
    init_resolved(level.unwrap_or("info"), tee_stderr);
}

/// TUI 专用:日志只写文件,不 tee stderr。
pub fn init_tui(level: Option<&str>) {
    init_with_log_level(level, false);
}

fn init_resolved(default_filter: &str, tee_stderr: bool) {
    let loader = shared::loader!(".");
    let path = loader
        .resolve("DATA::swift-ime.log")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/tmp/swift-ime.log".into());
    init_tracing(&path, default_filter, tee_stderr);
}

/// `ime_log!` —— 兼容旧调用点,统一走 `tracing::info!`(target `swift_ime`)。
/// 引擎(ime-core)与前端日志因此落在同一个文件、同一套级别过滤下。
#[macro_export]
macro_rules! ime_log {
    ($($arg:tt)*) => {
        tracing::info!(target: "swift_ime", $($arg)*)
    };
}
