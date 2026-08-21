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

use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::{Arc, Mutex, OnceLock, Weak};

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

/// C++ 传入的前端句柄(与 `swift-ime.h` 的 `FcitxHandle` 布局一致)。
///
/// `instance` 即 userdata(C++ `SwiftImeEngine::this`),两个回调由引擎 I/O
/// 线程调用、以 `instance` 作 userdata。函数指针可为空(`None` = 不注册,
/// 对应 C 侧 null)。
#[repr(C)]
pub struct FcitxHandle {
    pub instance: *mut c_void,
    pub refresh_ui: Option<RefreshCb>,
    pub get_clip_board: Option<ClipboardCb>,
}

/// 前端句柄实现:I/O 线程推送经 C 回调转到 fcitx 主循环。
///
/// `engine` 是引擎的 `Weak` 引用 —— `refresh_ui` 用它同步判断某个 ctx 是否
/// 仍有活跃的 #asr 会话(voice server 据此决定是否放弃)。
///
/// 引擎以泄漏的 `Arc`(`Arc::into_raw`)交给 C++ 持有,`swift_ime_destroy` 用
/// `Arc::from_raw` 回收。弱引用保证:即使 voice server 握着前端的强引用、
/// 引擎已先被销毁,`upgrade()` 也只会得到 `None` —— 不会解引用悬垂内存。
///
/// 两个回调由 `swift_ime_create` 一次性传入、构造后不可变 —— 直接存字段,
/// 无需 `Mutex`/`Arc` 间接(`extern "C" fn` + `usize` 都是 `Send + Sync`)。
struct FcitxFrontend {
    /// C++ 传入的刷新回调 + userdata(C 侧 `this`)。
    refresh: Option<(RefreshCb, usize)>,
    /// C++ 传入的剪贴板请求回调 + userdata。
    clipboard: Option<(ClipboardCb, usize)>,
    engine: OnceLock<Weak<ImeEngine>>,
}

impl FcitxFrontend {
    fn new(
        refresh: Option<(RefreshCb, usize)>,
        clipboard: Option<(ClipboardCb, usize)>,
    ) -> Self {
        FcitxFrontend {
            refresh,
            clipboard,
            engine: OnceLock::new(),
        }
    }

    /// 引擎完全构造后调用一次:把弱引用交给前端。
    fn attach_engine(&self, engine: &Arc<ImeEngine>) {
        let _ = self.engine.set(Arc::downgrade(engine));
    }
}

impl FrontEndHandle for FcitxFrontend {
    fn get_clipboard_item(&self, count: u32) {
        if let Some((cb, ud)) = self.clipboard {
            // userdata 在 LP64 上位宽等同 `void*` —— 位模式转换为裸指针传给 C ABI。
            cb(count, ud as *mut c_void);
        }
    }

    fn refresh_ui(&self, sv: StateView) -> bool {
        if let Some((cb, ud)) = self.refresh {
            cb(sv.ctx, ud as *mut c_void);
        }
        // 同步判定:该 ctx 是否还有活跃的 #asr 会话。voice server 收到 false
        // 即放弃、不重连。引擎未接上 / 已销毁 → 保守"接受",不误杀。
        let alive = self
            .engine
            .get()
            .and_then(|w| w.upgrade())
            .map(|e| e.is_voice_ctx_alive(sv.ctx))
            .unwrap_or(true);
        tracing::info!(ctx = sv.ctx, alive, "FcitxFrontend::refresh_ui → C cb");
        alive
    }
}

/// 非语音候选行的显示字节上限(候选槽 buffer 是 `char[128]`,取 128 即不
/// 额外截断)。语音(`#asr`)候选在 [`truncate_candidate_rows`] 里单独截到 ≤8 字。
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
pub extern "C" fn swift_ime_create(
    _config_path: *const c_char,
    handle: *const FcitxHandle,
) -> *mut ImeEngine {
    // 先读配置(拿 debug.log_level),再装进程级 tracing subscriber —— 之后
    // 的 ime_log! / 引擎 tracing 事件统一写进 swift-ime.log。
    let cfg = crate::config::SwiftImeConfig::load();
    crate::logger::init_with_log_level(cfg.debug.log_level.as_deref(), true);
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
    // 回调打包在 FcitxHandle 里由 C++ 传入 —— 无全局注册表、无 set_ui_cbs。
    let h = unsafe { handle.as_ref() };
    let ud = h.map(|h| h.instance as usize).unwrap_or(0);
    let frontend = Arc::new(FcitxFrontend::new(
        h.and_then(|h| h.refresh_ui).map(|cb| (cb, ud)),
        h.and_then(|h| h.get_clip_board).map(|cb| (cb, ud)),
    ));
    let mut engine = Arc::new(ImeEngine::with_config(
        weights,
        eng_weights,
        Box::new(FcitxProvider::default()),
        snippets,
        cfg.weights.to_scoring(),
        frontend.clone() as Arc<dyn FrontEndHandle>,
        cfg.voice.aura_base.clone(),
    ));
    // &mut 配置 —— 必须在 attach_engine(建 Weak)之前:Arc::get_mut 要求
    // strong_count==1 且 weak_count==0。先配置完,再 attach。万一不变量被破坏,
    // 降级用默认配置(不 panic,IME 里 panic 会拖垮整个 fcitx5)。
    if let Some(eng) = Arc::get_mut(&mut engine) {
        eng.set_page_size(cfg.input.page_size);
        eng.set_context_aware(cfg.input.context_aware);
        eng.set_candidate_meta(cfg.debug.candidate_meta);
        eng.set_req_base(&cfg.magic.req_base);
    } else {
        tracing::error!(target: "swift_ime", "engine not sole owner at config time — 应用默认配置");
    }
    // 引擎已完全构造,再把弱引用交给前端做 is_voice_ctx_alive。
    frontend.attach_engine(&engine);

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

    // 泄漏的 Arc 交给 C++ 持有;`swift_ime_destroy` 用 Arc::from_raw 回收。
    Arc::into_raw(engine) as *mut ImeEngine
}

#[no_mangle]
pub extern "C" fn swift_ime_destroy(engine: *mut ImeEngine) {
    if engine.is_null() { return; }
    // 回收 into_raw 泄漏的强引用 → 若无其它强引用则释放引擎(前端随之 drop)。
    // 与 voice server 并发时由 Arc 引用计数兜底,不会 UAF。
    unsafe { drop(Arc::from_raw(engine as *const ImeEngine)); }
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
    // 排查流式不刷新:C++ drain 是否在调这里、结果如何(Some=重建视图 / None=跳过)。
    tracing::info!(ctx = c, "swift_ime_magic_tick");
    match unsafe { &*engine }.magic_tick_ctx(c) {
        Some(mut view) => {
            tracing::info!(ctx = c, count = view.candidate_count, "magic_tick → Some");
            truncate_candidate_rows(&mut view);
            unsafe { *out_view = view; }
            1
        }
        None => {
            tracing::info!(ctx = c, "magic_tick → None");
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
    fn short_rows_untouched() {
        let short = "你好";
        let mut view = ImeView::empty();
        ImeView::set_str(&mut view.candidates[0].text, short);
        view.candidate_count = 1;
        truncate_candidate_rows(&mut view);
        assert_eq!(ImeView::str_field(&view.candidates[0].text), short);
    }
}
