//! C ABI wrapper over `ime-core`. Built as a cdylib (`libime_core.so`) and linked by
//! the fcitx5 C++ thin glue in `apps/swift-ime/glue/`.
//!
//! The public API is 7 `extern "C"` functions. The C++ glue calls these directly;
//! `cbindgen` generates the matching header (`swift_ime_ffi.h`) from this file.
//!
//! This crate is independent of the `swift-ime` binary — it only wraps `ime-core`.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::Mutex;

use ime_core::{
    Dispatcher, Expander, ImeState, Matcher, SnippetStore,
    expander::StaticProvider,
    platform::NoopPinyin,
};

// ── C ABI types (cbindgen exports these) ───────────────────────────────

/// Action returned from process_key.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImeActionFFI {
    PassThrough = 0,
    Preedit    = 1,
    Commit     = 2,
    Candidates = 3,
}

/// One candidate entry.
#[repr(C)]
pub struct CandidateFFI {
    pub label:   [u8; 16],
    pub preview: [u8; 64],
}

pub const MAX_CANDIDATES: usize = 9;

// ── Global state ────────────────────────────────────────────────────────

struct State {
    dispatcher: Dispatcher,
    ime_state: ImeState,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

// ── C ABI entry points ──────────────────────────────────────────────────

/// Initialise the engine. `config_path` is the path to a snippet JSON file
/// (may be NULL for built-in defaults). Returns 0 on success.
#[no_mangle]
pub extern "C" fn swift_ime_init(config_path: *const c_char) -> i32 {
    let store = load_store(config_path);
    let matcher = Matcher::new(store.entries());
    let expander = Expander::new(Box::new(StaticProvider {
        date: String::from("2026-07-23"),
        clipboard: String::new(),
    }));
    let dispatcher = Dispatcher::new(matcher, expander, Box::new(NoopPinyin));

    *STATE.lock().unwrap() = Some(State { dispatcher, ime_state: ImeState::default() });
    tracing::info!(snippets = store.len(), "ime-core-ffi initialised");
    0
}

/// Process one key event. `ch` is a Unicode scalar value.
/// Writes result text (commit/preedit) into `out_text` (up to `out_cap` bytes,
/// NUL-terminated). `out_len` receives the byte count written.
/// Returns the action type.
#[no_mangle]
pub extern "C" fn swift_ime_process_key(
    ch: u32,
    out_text: *mut u8,
    out_cap: u32,
    out_len: *mut u32,
) -> ImeActionFFI {
    let mut guard = STATE.lock().unwrap();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return ImeActionFFI::PassThrough,
    };

    let c = char::from_u32(ch).unwrap_or('\0');
    if c == '\0' {
        return ImeActionFFI::PassThrough;
    }

    let action = state.dispatcher.process_key(c, &mut state.ime_state);
    let (ffi, text) = translate(action);

    if !text.is_empty() && !out_text.is_null() && out_cap > 0 {
        unsafe { write_out(text.as_bytes(), out_text, out_cap, out_len); }
    } else if !out_len.is_null() {
        unsafe { *out_len = 0; }
    }

    ffi
}

/// Select a candidate by index.
#[no_mangle]
pub extern "C" fn swift_ime_select_candidate(index: u32) -> ImeActionFFI {
    let mut guard = STATE.lock().unwrap();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return ImeActionFFI::PassThrough,
    };
    let action = state.dispatcher.select_candidate(index as usize, &mut state.ime_state);
    translate(action).0
}

/// Fill `out_items` with candidates. Returns count written.
#[no_mangle]
pub extern "C" fn swift_ime_candidates(
    _out_items: *mut CandidateFFI,
    _max_items: u32,
) -> u32 {
    0 // Phase 3: pinyin candidates
}

#[no_mangle] pub extern "C" fn swift_ime_activate()   { tracing::debug!("activate"); }
#[no_mangle] pub extern "C" fn swift_ime_deactivate() { tracing::debug!("deactivate"); }

#[no_mangle]
pub extern "C" fn swift_ime_reset() {
    if let Some(state) = STATE.lock().unwrap().as_mut() {
        state.dispatcher.reset(&mut state.ime_state);
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn load_store(config_path: *const c_char) -> SnippetStore {
    let json = if config_path.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(config_path) }.to_string_lossy().into_owned()
    };
    if !json.is_empty() {
        if let Ok(store) = SnippetStore::from_json(&json) {
            return store;
        }
    }
    // Built-in fallback.
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
    {"trigger": "/sig",   "expand": "Best regards,\nAlice\n$DATE",          "desc": "邮件签名"}
]"##;
