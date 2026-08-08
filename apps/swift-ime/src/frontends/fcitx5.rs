//! fcitx5 frontend — C ABI entry points for the cdylib.
//!
//! Thin wrapper around [`ime_core::engine::ImeEngine`]. The C++ side
//! creates one ImeEngine per SwiftImeEngine instance and passes the
//! opaque pointer on every call. Per-context state is managed
//! internally by the engine.
//!
//! Candidate rows are truncated **here, at the frontend**: the engine passes
//! full texts (voice sentences are long), and each frontend renders rows to
//! its own space — the fcitx5 panel is short, so rows get a compact preview;
//! the TUI shows everything. Commit always uses the engine's full text, so
//! truncation never loses data.

use std::os::raw::c_char;
use std::sync::{Arc, Mutex};

use ime_core::asr_buffer::AsrBuffer;
use ime_core::engine::ImeEngine;
use ime_core::expander::{today_str, VariableProvider};
use ime_core::family::magic::preview_text;
use ime_core::platform::ImeView;

/// Max displayed bytes for one candidate row in the fcitx5 panel (≈8 CJK
/// chars). The panel adapts its width to the longest row — truncating here
/// keeps the box compact while the full text stays committable.
const FCITX_CANDIDATE_TEXT_MAX: usize = 8 * 3;

/// Real variable provider for the fcitx5 environment: a live `$DATE` and the
/// clipboard text pushed by the C++ glue (fcitx5 clipboard events) via
/// [`swift_ime_set_clipboard`]. `$CLIPBOARD` snippet templates resolve to the
/// current clipboard.
#[derive(Default)]
struct FcitxProvider {
    clipboard: Mutex<String>,
}

impl VariableProvider for FcitxProvider {
    fn resolve(&self, name: &str) -> Option<String> {
        match name {
            "DATE" => Some(today_str()),
            "CLIPBOARD" => Some(self.clipboard.lock().unwrap().clone()),
            _ => None,
        }
    }

    fn set(&self, name: &str, value: &str) {
        if name == "CLIPBOARD" {
            *self.clipboard.lock().unwrap() = value.to_string();
        }
    }
}

/// Truncate candidate row texts in place, at the frontend boundary. Pure
/// display — the engine's internal candidates (and thus what Space commits)
/// are untouched.
fn truncate_candidate_rows(view: &mut ImeView) {
    for i in 0..view.candidate_count as usize {
        let text = ImeView::str_field(&view.candidates[i].text);
        let preview = preview_text(&text, FCITX_CANDIDATE_TEXT_MAX);
        if preview != text {
            ImeView::set_str(&mut view.candidates[i].text, &preview);
        }
    }
}

// ── C ABI — all functions take the engine pointer ──────────────────────

#[no_mangle]
pub extern "C" fn swift_ime_create(_config_path: *const c_char) -> *mut ImeEngine {
    crate::logger::init_default();
    crate::ime_log!("swift-ime cdylib loaded");

    let cfg = crate::config::SwiftImeConfig::load();
    let weights = cfg.weights.pinyin.to_engine();
    let eng_weights = ime_core::family::english::EnglishWeights {
        exact: cfg.weights.english.exact,
        prefix_ratio: cfg.weights.english.prefix_ratio,
        user_boost: cfg.weights.english.user_boost,
    };
    let snippets: Vec<(String, String)> = cfg.snippets
        .iter()
        .map(|s| (s.trigger.clone(), s.expand.clone()))
        .collect();
    let mut engine = ImeEngine::with_config(
        weights,
        eng_weights,
        Box::new(FcitxProvider::default()),
        snippets,
        cfg.weights.to_scoring(),
    );
    // 候选每页条数(swift-ime.yaml → input.page_size)。
    engine.set_page_size(cfg.input.page_size);
    // 上下文感知开关(swift-ime.yaml → input.context_aware)。
    engine.set_context_aware(cfg.input.context_aware);
    // 调试模式:候选词显示提供者与权重。
    engine.set_candidate_meta(cfg.debug.candidate_meta);

    // `#req` backend base URL (config `magic.req_base`, default
    // http://127.0.0.1:14555/api).
    engine.set_req_base(&cfg.magic.req_base);

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

    // ── Emoji keyword table (CLDR-generated) + user mapping ──
    if cfg.dicts.emoji {
        let loader = shared::loader!("assets");
        if let Some(p) = loader.resolve("DICT::emoji.tsv") {
            match engine.load_emoji_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded emoji: {n} keyword rows from {}", p.display()),
                Err(e) => crate::ime_log!("emoji dict load error: {e}"),
            }
        }
    }
    let loader = shared::loader!(".");
    if let Some(p) = loader.resolve("CONF::emoji_user.tsv") {
        if p.exists() {
            match engine.load_emoji_user_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded {n} emoji user rows from {}", p.display()),
                Err(e) => crate::ime_log!("emoji user dict load error: {e}"),
            }
        }
    }

    // Initialize SQLite weight store — DATA 命名空间(dev: data/, prod: ~/.desk-pilot/)。
    let data = shared::loader!(".");
    let db = data.resolve("DATA::swift-ime.db")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data/swift-ime.db".into());
    engine.init_store(&db);

    // ── Voice input: spawn aura SSE client + attach buffer to engine ──
    let asr_buffer = Arc::new(AsrBuffer::new());
    engine.set_asr_buffer(Arc::clone(&asr_buffer));
    crate::bridge::spawn_aura_client(asr_buffer, None);
    // ──────────────────────────────────────────────────────────────────

    // ── English user dictionary ──
    let loader = shared::loader!(".");
    if let Some(p) = loader.resolve("CONF::en_user.tsv") {
        match engine.load_en_user_dict(&p.to_string_lossy()) {
            Ok(n) => crate::ime_log!("loaded {n} en user words from {}", p.display()),
            Err(e) => crate::ime_log!("en user dict load error: {e}"),
        }
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
    let mut view = unsafe { &*engine }.predict_ctx(ctx as usize, c);
    truncate_candidate_rows(&mut view);
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
    let mut view = unsafe { &*engine }.select_ctx(ctx as usize, index as usize);
    truncate_candidate_rows(&mut view);
    unsafe { *out_view = view; }
    1
}

#[no_mangle]
pub extern "C" fn swift_ime_reset(engine: *mut ImeEngine, ctx: *const std::ffi::c_void) {
    if engine.is_null() { return; }
    unsafe { &*engine }.reset_ctx(ctx as usize);
}

#[no_mangle]
pub extern "C" fn swift_ime_special_key(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    code: i32,
    out_view: *mut ImeView,
) -> i32 {
    if engine.is_null() || out_view.is_null() { return 0; }
    let mut view = unsafe { &*engine }.special_key_code_ctx(ctx as usize, code);
    truncate_candidate_rows(&mut view);
    unsafe { *out_view = view; }
    1
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
    let mut view = unsafe { &*engine }.commit_pending_ctx(ctx as usize);
    truncate_candidate_rows(&mut view);
    unsafe { *out_view = view; }
    1
}

#[no_mangle]
pub extern "C" fn swift_ime_set_surrounding(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    text: *const c_char,
) {
    if engine.is_null() || text.is_null() { return; }
    let s = unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy();
    if s.is_empty() { return; }
    unsafe { &*engine }.set_surrounding(ctx as usize, &s);
}

#[no_mangle]
pub extern "C" fn swift_ime_poll_async(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    out_view: *mut ImeView,
) -> i32 {
    if engine.is_null() || out_view.is_null() { return 0; }
    let (code, mut view) = unsafe { &*engine }.poll_async_ctx(ctx as usize);
    if code != 0 {
        truncate_candidate_rows(&mut view);
        unsafe { *out_view = view; }
    }
    code
}

/// Poll for changes while a live magic command (`#asr` voice anchor, `#req`
/// HTTP request, …) is active — the async candidate refresh entry point.
/// Returns 1 + fills `out_view` if the active member's async state advanced
/// and the ctx is in Magic mode; 0 otherwise. Called by the C++ glue's
/// periodic TimeEvent (main loop) so the candidate area updates live WITHOUT
/// a keypress — same `magic_tick` the TUI uses.
#[no_mangle]
pub extern "C" fn swift_ime_magic_tick(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    out_view: *mut ImeView,
) -> i32 {
    if engine.is_null() || out_view.is_null() { return 0; }
    match unsafe { &*engine }.magic_tick_ctx(ctx as usize) {
        Some(mut view) => {
            truncate_candidate_rows(&mut view);
            unsafe { *out_view = view; }
            1
        }
        None => 0,
    }
}

/// Reconfigure the `#req` backend base URL at runtime (default
/// http://127.0.0.1:14555/api). Safe to call any time — shared config.
#[no_mangle]
pub extern "C" fn swift_ime_set_req_base(
    engine: *mut ImeEngine,
    base: *const c_char,
) -> i32 {
    if engine.is_null() || base.is_null() { return 0; }
    let s = unsafe { std::ffi::CStr::from_ptr(base) }.to_string_lossy();
    unsafe { &*engine }.set_req_base(&s);
    1
}

/// Push the current clipboard text from the frontend (fcitx5 clipboard events).
/// `$CLIPBOARD` snippet templates resolve to the latest pushed value.
#[no_mangle]
pub extern "C" fn swift_ime_set_clipboard(
    engine: *mut ImeEngine,
    text: *const c_char,
) -> i32 {
    if engine.is_null() || text.is_null() { return 0; }
    let s = unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy();
    unsafe { &*engine }.set_variable("CLIPBOARD", &s);
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_rows_truncated_at_frontend_with_ellipsis() {
        // 24 CJK chars = 72 bytes, well above the 24-byte panel cap.
        let long = "今天天气真不错我们一起去公园散步顺便买个冰淇淋吃";
        let mut view = ImeView::empty();
        ImeView::set_str(&mut view.candidates[0].text, long);
        view.candidate_count = 1;
        truncate_candidate_rows(&mut view);
        let shown = ImeView::str_field(&view.candidates[0].text);
        assert!(shown.ends_with('…'), "row truncated with ellipsis: {shown}");
        assert_eq!(shown.len(), FCITX_CANDIDATE_TEXT_MAX + 3, "FCITX_CANDIDATE_TEXT_MAX bytes + 3-byte …: {shown}");
        assert!(shown.starts_with("今天天气真"), "head kept: {shown}");
    }

    #[test]
    fn short_rows_untouched() {
        let short = "你好";
        let mut view = ImeView::empty();
        ImeView::set_str(&mut view.candidates[0].text, short);
        view.candidate_count = 1;
        truncate_candidate_rows(&mut view);
        assert_eq!(ImeView::str_field(&view.candidates[0].text), short);
    }
}
