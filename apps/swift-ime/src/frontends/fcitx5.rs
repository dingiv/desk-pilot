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
//!
//! # Safety(FFI 边界)
//!
//! 本模块的全部公开函数是 C ABI 入口,裸指针(`*mut ImeEngine` /
//! `*const c_char`)由 C++ 胶水层(swift-ime.cpp)按 addon 契约持有并传入:
//! 每个 handle 由 `swift_ime_create` 唯一创建、`swift_ime_destroy` 唯一
//! 销毁,C 侧不存在悬垂/别名调用。Rust 侧的 `unsafe` 标记对 C 调用方
//! 无意义(C 无此概念),故在此关闭 clippy 的 not_unsafe_ptr_arg_deref ——
//! 该 lint 只对**Rust 调用方**有保护价值,而这里没有 Rust 调用方。
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::{Arc, Mutex};

use ime_core::engine::ImeEngine;
use ime_core::expander::{today_str, VariableProvider};
use ime_core::family::magic::preview_text;
use ime_core::frontend::{FrontEndHandle, StateView};
use ime_core::platform::ImeView;

// ── 前端句柄:C 回调转发(引擎 I/O 线程 → fcitx 主循环)──────────────────

/// C++ 注册的刷新回调:`ctx` = 输入上下文指针转 usize;`userdata` 是 C++ 侧
/// 传入的 `this`(ABI 与 `void*` 一致)。
type RefreshCb = extern "C" fn(ctx: usize, userdata: *mut c_void);
/// C++ 注册的剪贴板请求回调:引擎要 count 条历史。
type ClipboardCb = extern "C" fn(count: u32, userdata: *mut c_void);

/// 共享的 C 回调槽 —— 引擎持有 FrontEndHandle,注册表持有同一 Arc,`set_ui_cbs`
/// 之后 C++ 回调生效。
///
/// `userdata` 以 `usize` 位模式存储(`*mut c_void` 在 LP64 上位宽等同);
/// 调用前 cast 回 `*mut c_void` 即可。这样 `Mutex` 的内容是 `Send`(`extern fn`
/// + `usize` 都是),便于跨线程锁。
struct FcitxCbs {
    refresh: Mutex<Option<(RefreshCb, usize)>>,
    clipboard: Mutex<Option<(ClipboardCb, usize)>>,
}

/// 前端句柄实现:I/O 线程推送经 C 回调转到 fcitx 主循环。
struct FcitxFrontend {
    cbs: Arc<FcitxCbs>,
}

impl FrontEndHandle for FcitxFrontend {
    fn get_clipboard_item(&self, count: u32) {
        if let Some((cb, ud)) = *self.cbs.clipboard.lock().unwrap() {
            // userdata 在 LP64 上位宽等同 `void*` —— 位模式转换为裸指针传给 C ABI。
            cb(count, ud as *mut c_void);
        }
    }

    fn refresh_ui(&self, sv: StateView) {
        tracing::debug!(ctx = sv.ctx, "FcitxFrontend::refresh_ui → C cb");
        if let Some((cb, ud)) = *self.cbs.refresh.lock().unwrap() {
            cb(sv.ctx, ud as *mut c_void);
        }
    }
}

/// 引擎指针 → C 回调槽(供 `swift_ime_set_ui_cbs` 设置)。
static FRONTS: std::sync::OnceLock<Mutex<HashMap<usize, Arc<FcitxCbs>>>> = std::sync::OnceLock::new();

fn front_registry() -> &'static Mutex<HashMap<usize, Arc<FcitxCbs>>> {
    FRONTS.get_or_init(|| Mutex::new(HashMap::new()))
}

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
        let preview = preview_text(text, FCITX_CANDIDATE_TEXT_MAX);
        if preview != text {
            ImeView::set_str(&mut view.candidates[i].text, &preview);
        }
    }
}

// ── C ABI — all functions take the engine pointer ──────────────────────

#[no_mangle]
pub extern "C" fn swift_ime_create(_config_path: *const c_char) -> *mut ImeEngine {
    // 先读配置(拿 debug.log_level),再装进程级 tracing subscriber —— 之后
    // 的 ime_log! / 引擎 tracing 事件统一写进 swift-ime.log。
    let cfg = crate::config::SwiftImeConfig::load();
    crate::logger::init_with_log_level(cfg.debug.log_level.as_deref());
    crate::ime_log!("swift-ime cdylib loaded");
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
    // 前端句柄:C 回调槽,稍后由 `swift_ime_set_ui_cbs` 注入。
    let cbs = Arc::new(FcitxCbs {
        refresh: Mutex::new(None),
        clipboard: Mutex::new(None),
    });
    let mut engine = ImeEngine::with_config(
        weights,
        eng_weights,
        Box::new(FcitxProvider::default()),
        snippets,
        cfg.weights.to_scoring(),
        Arc::new(FcitxFrontend { cbs: Arc::clone(&cbs) }),
        cfg.voice.aura_base.clone(),
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
                Err(e) => tracing::error!(target: "swift_ime", "loading rime-ice: {e}"),
            }
        } else {
            tracing::warn!(target: "swift_ime", "rime-ice.fst not found");
        }
    }

    // ── Emoji 家族开关:dicts.emoji: false → 整个家族禁用(无 emoji 候选)──
    if !cfg.dicts.emoji {
        engine.set_family_enabled("emoji", false);
    }

    // ── Emoji keyword table (CLDR-generated) + user mapping ──
    if cfg.dicts.emoji {
        let loader = shared::loader!("assets");
        if let Some(p) = loader.resolve("DICT::emoji.tsv") {
            match engine.load_emoji_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded emoji: {n} keyword rows from {}", p.display()),
                Err(e) => tracing::warn!(target: "swift_ime", "emoji dict load error: {e}"),
            }
        }
    }
    let loader = shared::loader!(".");
    if let Some(p) = loader.resolve("CONF::emoji_user.tsv") {
        if p.exists() {
            match engine.load_emoji_user_dict(&p.to_string_lossy()) {
                Ok(n) => crate::ime_log!("loaded {n} emoji user rows from {}", p.display()),
                Err(e) => tracing::warn!(target: "swift_ime", "emoji user dict load error: {e}"),
            }
        }
    }

    // Initialize SQLite weight store — DATA 命名空间(dev: data/, prod: ~/.desk-pilot/)。
    let data = shared::loader!(".");
    let db = data.resolve("DATA::swift-ime.db")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data/swift-ime.db".into());
    engine.init_store(&db);

    // ── Voice input: voice listener 在 `ImeEngine::with_config` 内部启动,
    // 跟随引擎 drop 自动清理。无需此处显式接线。 ──

    // ── English user dictionary ──
    let loader = shared::loader!(".");
    if let Some(p) = loader.resolve("CONF::en_user.tsv") {
        match engine.load_en_user_dict(&p.to_string_lossy()) {
            Ok(n) => crate::ime_log!("loaded {n} en user words from {}", p.display()),
            Err(e) => tracing::warn!(target: "swift_ime", "en user dict load error: {e}"),
        }
    }

    // 注册 C 回调槽(引擎指针 → cbs),`swift_ime_set_ui_cbs` 据此设置。
    let engine_ptr = Box::into_raw(Box::new(engine));
    front_registry().lock().unwrap().insert(engine_ptr as usize, cbs);
    engine_ptr
}

#[no_mangle]
pub extern "C" fn swift_ime_destroy(engine: *mut ImeEngine) {
    if engine.is_null() { return; }
    front_registry().lock().unwrap().remove(&(engine as usize));
    unsafe { drop(Box::from_raw(engine)); }
}

/// C ABI 的键事件包 —— C++ 胶水**忠实组包**(keysym + unicode + 修饰键
/// 状态),不做任何拦截或映射;键类归一与修饰键策略全部由引擎的输入
/// 路由层决定。字段布局必须与 swift-ime.h 的 SwiftKeyPacket 一致。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CKeyEvent {
    /// X keysym(FcitxKey_*;ASCII 可打印区与 unicode 同值)。
    pub sym: u32,
    /// `keySymToUnicode(sym)`,无映射时为 0。
    pub unicode: u32,
    pub ctrl: u8,
    pub shift: u8,
    pub alt: u8,
}

/// 统一键入口:所有键(含特殊键与 Ctrl/Shift/Alt 状态)都走这里。
/// 返回的 `ImeView::action` 告诉 C++ 侧如何反应 —— `HANDLED` 未置位即
/// 不 filterAndAccept,键自然到达应用。
#[no_mangle]
pub extern "C" fn swift_ime_key(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    ev: *const CKeyEvent,
    out_view: *mut ImeView,
) -> i32 {
    if engine.is_null() || out_view.is_null() || ev.is_null() { return 0; }
    let ev = unsafe { *ev };
    let key = ime_core::router::KeyEvent::from_fcitx(
        ev.sym, ev.unicode,
        ev.ctrl != 0, ev.shift != 0, ev.alt != 0,
    );
    let mut view = unsafe { &*engine }.key_ctx(ctx as usize, key);
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

/// 注册前端 UI 回调(C++ 在引擎创建后调用一次):
/// - `refresh_cb(ctx, userdata)`:引擎 I/O 线程异步状态推进 → 前端主循环拉视图;
/// - `clipboard_cb(count, userdata)`:引擎请求剪贴板历史 → 前端取到回填。
///
/// `userdata` 是 C++ 侧 `SwiftImeEngine::this`,指针宽(64-bit on LP64),
/// 回调在引擎 I/O 线程执行,但 C++ 内部仅用它定位 `this` + marshal 到
/// fcitx 主循环,不直接触碰 Rust 状态。
#[no_mangle]
pub extern "C" fn swift_ime_set_ui_cbs(
    engine: *mut ImeEngine,
    refresh_cb: RefreshCb,
    clipboard_cb: ClipboardCb,
    userdata: *mut c_void,
) -> i32 {
    if engine.is_null() { return 0; }
    let key = engine as usize;
    let reg = front_registry().lock().unwrap();
    let Some(cbs) = reg.get(&key).cloned() else { return 0; };
    // userdata 以 usize 位模式存储(LP64 上等同 void*),便于跨线程锁。
    let ud = userdata as usize;
    *cbs.refresh.lock().unwrap() = Some((refresh_cb, ud));
    *cbs.clipboard.lock().unwrap() = Some((clipboard_cb, ud));
    1
}

/// 拉取当前 live 视图(异步状态推进后,前端主循环经 refresh 回调调这里)。
/// 返回 1 + 填 `out_view` 若该 ctx 有 live 命令且状态推进;否则 0。
#[no_mangle]
pub extern "C" fn swift_ime_magic_tick(
    engine: *mut ImeEngine,
    ctx: *const std::ffi::c_void,
    out_view: *mut ImeView,
) -> i32 {
    if engine.is_null() || out_view.is_null() { return 0; }
    let c = ctx as usize;
    tracing::debug!(ctx = c, "swift_ime_magic_tick");
    match unsafe { &*engine }.magic_tick_ctx(c) {
        Some(mut view) => {
            tracing::debug!(ctx = c, count = view.candidate_count, "magic_tick → Some");
            truncate_candidate_rows(&mut view);
            unsafe { *out_view = view; }
            1
        }
        None => {
            tracing::debug!(ctx = c, "magic_tick → None");
            0
        }
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
