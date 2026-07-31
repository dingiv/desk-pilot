//! ImeLogger — unified logging for swift-ime.
//!
//! Reads log path from config. In dev builds, tees to both file and stderr.
//! In release builds, writes to file only. Truncates the file if it exceeds
//! `max_bytes` (default 2MB) on open.
//!
//! ```ignore
//! use logger::ime_log;
//!
//! ime_log!("loaded {n} dict entries");
//! ime_log!("ERROR: {e}");
//! ```

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;

static LOGGER: OnceLock<Mutex<ImeLogger>> = OnceLock::new();

struct ImeLogger {
    file: Option<std::fs::File>,
    tee_stderr: bool,
}

impl ImeLogger {
    fn open(path: &str, tee_stderr: bool, max_bytes: u64) -> Self {
        // Truncate if too large.
        if let Ok(meta) = fs::metadata(path) {
            if meta.len() > max_bytes {
                let _ = fs::write(path, "");
            }
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
        ImeLogger { file, tee_stderr }
    }

    fn write_line(&mut self, msg: &str) {
        if self.tee_stderr {
            eprintln!("{msg}");
        }
        if let Some(ref mut f) = self.file {
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// Initialize the logger. Call once at startup.
///
/// - `path`: log file path (e.g. "~/.desk-pilot/swift-ime.log")
/// - `tee_stderr`: if true, also print to stderr (dev mode)
/// - `max_bytes`: truncate file if larger than this (default 2MB)
pub fn init(path: &str, tee_stderr: bool, max_bytes: u64) {
    let logger = ImeLogger::open(path, tee_stderr, max_bytes);
    let _ = LOGGER.set(Mutex::new(logger));
}

/// Initialize with sensible defaults.
/// Path: ~/.desk-pilot/swift-ime.log (resolved via HOME env).
/// Dev: also tees to stderr. Release: file only.
pub fn init_default() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let path = format!("{home}/.desk-pilot/swift-ime.log");
    init(&path, cfg!(debug_assertions), 2 * 1024 * 1024);
}

/// Initialize from FileLoader CONF namespace (path: CONF::swift-ime.log).
pub fn init_from_loader() {
    let loader = shared::loader!(".");
    let tee = cfg!(debug_assertions);
    if let Some(p) = loader.resolve("CONF::swift-ime.log") {
        init(&p.to_string_lossy(), tee, 2 * 1024 * 1024);
    } else {
        init_default();
    }
}

/// Write a log line. No-op if logger not initialized.
pub fn log(msg: &str) {
    if let Some(m) = LOGGER.get() {
        if let Ok(mut logger) = m.lock() {
            logger.write_line(msg);
        }
    }
}

/// Macro for convenient logging with format strings.
#[macro_export]
macro_rules! ime_log {
    ($($arg:tt)*) => {
        $crate::logger::log(&format!($($arg)*))
    };
}
