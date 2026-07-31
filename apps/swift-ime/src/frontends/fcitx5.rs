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
    crate::logger::init_default();
    crate::ime_log!("swift-ime cdylib loaded");

    let cfg = crate::config::SwiftImeConfig::load();
    let weights = cfg.weights.pinyin.to_engine();
    let engine = ImeEngine::with_pinyin_weights(weights);

    // Load rime-ice FST if enabled in config.
    if cfg.dicts.rime_ice {
        let loader = shared::loader!("assets");
        if let Some(p) = loader.resolve("DICT::rime-ice.fst") {
            match engine.load_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded rime-ice: {n} entries from {}", p.display()),
                Err(e) => crate::ime_log!("ERROR loading rime-ice: {e}"),
            }
        } else {
            crate::ime_log!("rime-ice.fst not found");
        }
    }

    // Initialize SQLite weight store.
    let home = std::env::var("HOME").unwrap_or_default();
    engine.init_store(&format!("{home}/.desk-pilot/swift-ime.db"));

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
