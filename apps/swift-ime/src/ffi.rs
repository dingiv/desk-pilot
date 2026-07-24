//! C ABI wrapper over `ime-core`. Built as a cdylib, linked by the fcitx5 C++ glue.
//!
//! Per-window isolation: every C ABI function takes a `ctx` pointer (the fcitx5
//! `InputContext*`). The Rust side maintains a `HashMap<usize, StateMachine>` keyed
//! by that pointer — each window gets its own independent composition state. The
//! `Dispatcher` (engine) is initialised once and shared across all contexts.

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::{LazyLock, Mutex, OnceLock};

use ime_core::{
    Dispatcher, Expander, Matcher, SnippetStore,
    expander::StaticProvider,
    state::StateMachine,
};

// ── C ABI types ───────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImeActionFFI {
    PassThrough = 0,
    Preedit    = 1,
    Commit     = 2,
    Candidates = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CandidateFFI {
    pub text: [u8; 64],
}

pub const MAX_CANDIDATES: usize = 9;

// ── Global state ──────────────────────────────────────────────────────────

/// Engine pieces (Matcher, Expander, PinyinEngine) — built once at init, shared
/// across all input contexts. Thread-safe after construction.
static DISPATCHER: OnceLock<Dispatcher> = OnceLock::new();

/// Per-window composition states, keyed by the fcitx5 `InputContext*` pointer.
/// `HashMap::new()` is not const-stable — use `LazyLock` for lazy init.
static CONTEXTS: LazyLock<Mutex<HashMap<usize, StateMachine>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_ctx<T>(ctx: *const ::std::ffi::c_void, f: impl FnOnce(&Dispatcher, &mut StateMachine) -> T) -> T {
    let mut map = CONTEXTS.lock().unwrap();
    let sm = map.entry(ctx as usize).or_default();
    let disp = DISPATCHER.get().expect("swift-ime not initialised");
    f(disp, sm)
}

fn with_ctx_ref<T>(ctx: *const ::std::ffi::c_void, f: impl FnOnce(&Dispatcher, &StateMachine) -> T) -> T {
    let map = CONTEXTS.lock().unwrap();
    let sm = map.get(&(ctx as usize)).expect("unknown context");
    let disp = DISPATCHER.get().expect("swift-ime not initialised");
    f(disp, sm)
}

// ── C ABI entry points ────────────────────────────────────────────────────

/// Global init — called once when the addon is loaded.
#[no_mangle]
pub extern "C" fn swift_ime_init(config_path: *const c_char) -> i32 {
    let store = load_store(config_path);
    let matcher = Matcher::new(store.entries());
    let expander = Expander::new(Box::new(StaticProvider {
        date: String::from("2026-07-23"),
        clipboard: String::new(),
    }));
    let _ = DISPATCHER.set(Dispatcher::new(matcher, expander));
    tracing::info!(snippets = store.len(), "swift-ime initialised (per-context state)");
    0
}

/// Process one key event for context `ctx` (fcitx5 InputContext*).
#[no_mangle]
pub extern "C" fn swift_ime_process_key(
    ctx: *const ::std::ffi::c_void,
    ch: u32,
    out_text: *mut u8,
    out_cap: u32,
    out_len: *mut u32,
) -> ImeActionFFI {
    let c = char::from_u32(ch).unwrap_or('\0');
    if c == '\0' { return ImeActionFFI::PassThrough; }

    let (ffi, mut text) = with_ctx(ctx, |disp, sm| {
        let action = disp.process_key(c, sm);
        let (f, t) = translate(action);
        // Candidates: pass the preedit (what the app shows) as out_text so
        // the C++ glue can display it above the candidate window.
        let t = if f == ImeActionFFI::Candidates { sm.preedit.clone() } else { t };
        (f, t)
    });

    if !text.is_empty() && !out_text.is_null() && out_cap > 0 {
        unsafe { write_out(text.as_bytes(), out_text, out_cap, out_len); }
    } else if !out_len.is_null() {
        unsafe { *out_len = 0; }
    }
    ffi
}

#[no_mangle]
pub extern "C" fn swift_ime_select_candidate(
    ctx: *const ::std::ffi::c_void,
    index: u32,
    out_text: *mut u8,
    out_cap: u32,
    out_len: *mut u32,
) -> ImeActionFFI {
    let (ffi, text) = with_ctx(ctx, |disp, sm| {
        translate(disp.select_candidate(index as usize, sm))
    });
    if !text.is_empty() && !out_text.is_null() && out_cap > 0 {
        unsafe { write_out(text.as_bytes(), out_text, out_cap, out_len); }
    } else if !out_len.is_null() {
        unsafe { *out_len = 0; }
    }
    ffi
}

#[no_mangle]
pub extern "C" fn swift_ime_candidates(
    ctx: *const ::std::ffi::c_void,
    out_items: *mut CandidateFFI,
    max_items: u32,
) -> u32 {
    let cands = with_ctx_ref(ctx, |_disp, sm| {
        sm.candidates.clone()
    });
    if out_items.is_null() || max_items == 0 {
        return cands.len().min(max_items as usize) as u32;
    }
    let n = cands.len().min(max_items as usize);
    for i in 0..n {
        unsafe {
            let item = &mut *out_items.add(i);
            item.text.fill(0);
            let bytes = cands[i].as_bytes();
            let copy = bytes.len().min(item.text.len() - 1);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), item.text.as_mut_ptr(), copy);
        }
    }
    n as u32
}

#[no_mangle]
pub extern "C" fn swift_ime_activate(_ctx: *const ::std::ffi::c_void) {
    tracing::debug!("activate");
}

#[no_mangle]
pub extern "C" fn swift_ime_deactivate(ctx: *const ::std::ffi::c_void) {
    // Only clean up when the user explicitly switches away from our IME.
    // FocusOut (window defocus) does NOT reach here — the C++ glue filters
    // event.type() and only calls this on IM-switch.
    CONTEXTS.lock().unwrap().remove(&(ctx as usize));
    tracing::debug!("deactivate + removed context");
}

/// Called by the C++ glue on IM-switch: commit whatever is currently in the
/// composition buffer so no text is lost when switching away.
#[no_mangle]
pub extern "C" fn swift_ime_commit_pending(
    ctx: *const ::std::ffi::c_void,
    out_text: *mut u8,
    out_cap: u32,
    out_len: *mut u32,
) {
    let text = with_ctx_ref(ctx, |_disp, sm| {
        if sm.candidates.first().is_some() {
            sm.candidates[0].clone()
        } else if !sm.buffer.is_empty() {
            sm.buffer.clone()
        } else {
            String::new()
        }
    });
    if !text.is_empty() && !out_text.is_null() && out_cap > 0 {
        unsafe { write_out(text.as_bytes(), out_text, out_cap, out_len); }
    } else if !out_len.is_null() {
        unsafe { *out_len = 0; }
    }
}

#[no_mangle]
pub extern "C" fn swift_ime_reset(ctx: *const ::std::ffi::c_void) {
    with_ctx(ctx, |disp, sm| disp.reset(sm));
}

// ── helpers ───────────────────────────────────────────────────────────────

fn load_store(config_path: *const c_char) -> SnippetStore {
    let json = if config_path.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(config_path) }.to_string_lossy().into_owned()
    };
    if !json.is_empty() {
        if let Ok(store) = SnippetStore::from_json(&json) { return store; }
    }
    SnippetStore::from_json(DEFAULT_SNIPPETS).unwrap_or_else(|_| SnippetStore::new())
}

fn translate(action: ime_core::ImeAction) -> (ImeActionFFI, String) {
    match action {
        ime_core::ImeAction::PassThrough          => (ImeActionFFI::PassThrough, String::new()),
        ime_core::ImeAction::Preedit { text, .. } => (ImeActionFFI::Preedit,    text),
        ime_core::ImeAction::Commit(text)         => (ImeActionFFI::Commit,     text),
        ime_core::ImeAction::Candidates { .. }     => (ImeActionFFI::Candidates, String::new()),
    }
}

unsafe fn write_out(text: &[u8], out: *mut u8, cap: u32, out_len: *mut u32) {
    let n = text.len().min(cap as usize - 1);
    std::ptr::copy_nonoverlapping(text.as_ptr(), out, n);
    *out.add(n) = 0;
    *out_len = n as u32;
}

const DEFAULT_SNIPPETS: &str = r##"[
    {"trigger": "/greet", "expand": "你好，我是 AI 秘书，请问有什么可以帮你的？", "desc": "通用问候语"},
    {"trigger": "/sig",   "expand": "Best regards,\nAlice\n$DATE",          "desc": "邮件签名"},
    {"trigger": "#date",  "expand": "2026-07-23",                             "desc": "今日日期（固定）"}
]"##;
