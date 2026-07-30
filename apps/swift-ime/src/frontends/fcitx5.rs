//! fcitx5 frontend — C ABI entry points for the cdylib.
//!
//! Thin wrapper around [`ime_core::engine::ImeEngine`]. The C++ side
//! creates one ImeEngine per SwiftImeEngine instance and passes the
//! opaque pointer on every call. Per-context state is managed
//! internally by the engine.

use std::os::raw::c_char;

use ime_core::engine::ImeEngine;
use ime_core::platform::ImeView;

// ── C ABI — all functions take the engine pointer ──────────────────────

#[no_mangle]
pub extern "C" fn swift_ime_create(_config_path: *const c_char) -> *mut ImeEngine {
    let cfg = crate::config::SwiftImeConfig::load();
    let engine = ImeEngine::new();

    // Load rime-ice if enabled in config.
    if cfg.dicts.rime_ice {
        let loader = shared::loader!("assets");
        // Try FST first (fast load), fall back to TSV.
        let mut paths: Vec<std::path::PathBuf> = vec![
            "/usr/share/swift-ime/dict/rime-ice.fst".into(),
            "/usr/local/share/swift-ime/dict/rime-ice.fst".into(),
            "/usr/share/swift-ime/dict/rime-ice.tsv".into(),
        ];

        let log_dir = std::env::var("HOME").unwrap_or_default();
        let log_path = format!("{log_dir}/.desk-pilot/swift-ime-dict.log");
        let _ = std::fs::create_dir_all(std::path::Path::new(&log_path).parent().unwrap());

        let mut loaded = false;
        for p in &paths {
            let exists = p.exists();
            let _ = std::fs::write(&log_path, format!("probe: {} (exists={exists})", p.display()));
            if exists {
                match engine.load_dict(&p.to_string_lossy()) {
                    Ok(n) => {
                        let _ = std::fs::write(&log_path, format!("OK: {n} entries from {}", p.display()));
                        loaded = true; break;
                    }
                    Err(e) => {
                        let _ = std::fs::write(&log_path, format!("ERROR: {e}"));
                    }
                }
            }
        }
        if !loaded {
            let tried: Vec<_> = paths.iter().map(|p| format!("{} (e={})", p.display(), p.exists())).collect();
            let _ = std::fs::write(&log_path, format!("NOT FOUND. tried: {tried:?}"));
        }
    }

    // Restore user model from disk (pins + pick counters → per-family ranking).
    let l0_loader = shared::loader!(".");
    if let Some(l0_path) = l0_loader.resolve("CONF::swift-ime-l0.json") {
        engine.init_l0(&l0_path.to_string_lossy());
    }

    Box::into_raw(Box::new(engine))
}

#[no_mangle]
pub extern "C" fn swift_ime_destroy(engine: *mut ImeEngine) {
    if engine.is_null() { return; }
    unsafe { drop(Box::from_raw(engine)); }
}

#[no_mangle]
pub extern "C" fn swift_ime_process_key(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    ch: u32,
    out_view: *mut ImeView,
) -> i32 {
    let c = char::from_u32(ch).unwrap_or('\0');
    if c == '\0' || engine.is_null() || out_view.is_null() { return 0; }
    let view = unsafe { &*engine }.predict_ctx(ctx as usize, c);
    unsafe { *out_view = view; }
    1
}

#[no_mangle]
pub extern "C" fn swift_ime_select_candidate(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    index: u32,
    out_view: *mut ImeView,
) -> i32 {
    if engine.is_null() || out_view.is_null() { return 0; }
    let view = unsafe { &*engine }.select_ctx(ctx as usize, index as usize);
    unsafe { *out_view = view; }
    1
}

#[no_mangle]
pub extern "C" fn swift_ime_reset(engine: *mut ImeEngine, ctx: *const std::ffi::c_void) {
    if engine.is_null() { return; }
    unsafe { &*engine }.reset_ctx(ctx as usize);
}

#[no_mangle]
pub extern "C" fn swift_ime_activate(_engine: *mut ImeEngine, _ctx: *const std::ffi::c_void) {
    // No-op: ImeEngine lazily initializes per-context state on first use.
}

#[no_mangle]
pub extern "C" fn swift_ime_deactivate(engine: *mut ImeEngine, ctx: *const std::ffi::c_void) {
    if engine.is_null() { return; }
    unsafe { &*engine }.deactivate_ctx(ctx as usize);
}

#[no_mangle]
pub extern "C" fn swift_ime_commit_pending(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    out_view: *mut ImeView,
) -> i32 {
    if engine.is_null() || out_view.is_null() { return 0; }
    let view = unsafe { &*engine }.commit_pending_ctx(ctx as usize);
    unsafe { *out_view = view; }
    1
}

#[no_mangle]
pub extern "C" fn swift_ime_poll_async(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    out_view: *mut ImeView,
) -> i32 {
    if engine.is_null() || out_view.is_null() { return 0; }
    let (code, view) = unsafe { &*engine }.poll_async_ctx(ctx as usize);
    if code != 0 { unsafe { *out_view = view; } }
    code
}
