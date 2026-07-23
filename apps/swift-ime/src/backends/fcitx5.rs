//! fcitx5 backend — the Rust half of the fcitx5 addon. Per-process singleton that holds
//! the [`Dispatcher`] and [`ImeState`]. The C++ thin glue (`glue/engine.cpp`) calls the
//! `extern "C"` entry points below; this module translates between the C ABI types and
//! `ime_core` types.
//!
//! The C ABI entry points are only compiled when building as a shared library (cdylib).
//! For the binary target (mock/ibus modes), they are excluded.
//!
//! Thread-safety: `keyEvent` is called on fcitx5's main thread. All C ABI functions use
//! a global `Mutex<Fcitx5State>`. Network I/O (SSE, familiar socket) runs on background
//! threads spawned in `ime_init()`.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;

use ime_core::{Dispatcher, Expander, ImeState, Matcher, SnippetStore};
use ime_core::platform::NoopPinyin;
use ime_core::expander::StaticProvider;

use super::{CandidateFFI, ImeActionFFI, translate_action};

/// The mutable engine state behind a global Mutex. Initialised once in `ime_init()`.
#[allow(dead_code)]
struct Fcitx5State {
    dispatcher: Dispatcher,
    ime_state: ImeState,
    /// Pre-allocated buffers for FFI (avoid alloc in key event path).
    commit_buf: CString,
    preedit_buf: CString,
}

static STATE: Mutex<Option<Fcitx5State>> = Mutex::new(None);

// ── C ABI entry points (called from C++ glue) ──────────────────────────

/// Called once when the fcitx5 addon is loaded. `config_path` is the path to
/// `ime.json` (snippets config). Returns 0 on success, -1 on failure.
#[no_mangle]
pub extern "C" fn swift_ime_init(config_path: *const c_char) -> i32 {
    let path = if config_path.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(config_path) }.to_string_lossy().into_owned()
    };

    // Load snippet store from config, or use built-in defaults.
    let store = if !path.is_empty() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| SnippetStore::from_json(&s).ok())
            .unwrap_or_else(|| default_store())
    } else {
        default_store()
    };

    let matcher = Matcher::new(store.entries());
    let expander = Expander::new(Box::new(StaticProvider {
        date: chrono_date(),
        clipboard: String::new(),
    }));
    let dispatcher = Dispatcher::new(matcher, expander, Box::new(NoopPinyin));

    let state = Fcitx5State {
        dispatcher,
        ime_state: ImeState::default(),
        commit_buf: CString::new(String::with_capacity(4096)).unwrap_or_default(),
        preedit_buf: CString::new(String::with_capacity(256)).unwrap_or_default(),
    };
    *STATE.lock().unwrap() = Some(state);

    tracing::info!(snippet_count = store.len(), "swift-ime fcitx5 backend initialised");

    // TODO Phase 2: spawn background threads for SSE (aura) and TCP server (familiar)
    // std::thread::spawn(|| bridge::aura_sse_loop());
    // std::thread::spawn(|| bridge::familiar_server_loop());

    0
}

/// Process one key event. `ch` is the Unicode character (fcitx5 C++ glue does
/// key→char mapping via `FcitxKey::to_utf8()`). Returns the action type;
/// the committed / preedit text is written into `out_text` (caller must provide
/// a buffer of at least `out_cap` bytes). `out_text_len` receives the written
/// byte count (not including the NUL terminator, though one IS written).
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
    let action = state.dispatcher.process_key(c, &mut state.ime_state);

    let (ffi, text) = translate_action(action);
    if !text.is_empty() && !out_text.is_null() && out_cap > 0 {
        let bytes = text.as_bytes();
        let n = bytes.len().min(out_cap as usize - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_text, n);
            *out_text.add(n) = 0; // NUL terminate
            *out_len = n as u32;
        }
    } else if !out_len.is_null() {
        unsafe { *out_len = 0; }
    }

    ffi
}

/// User selected a candidate at `index`.
#[no_mangle]
pub extern "C" fn swift_ime_select_candidate(index: u32) -> ImeActionFFI {
    let mut guard = STATE.lock().unwrap();
    let state = match guard.as_mut() {
        Some(s) => s,
        None => return ImeActionFFI::PassThrough,
    };
    let action = state.dispatcher.select_candidate(index as usize, &mut state.ime_state);
    translate_action(action).0
}

/// Fill `out_items` with up to `max_items` candidate entries. Returns the count
/// written (currently always 0 — Phase 3 adds pinyin candidates).
#[no_mangle]
pub extern "C" fn swift_ime_candidates(
    _out_items: *mut CandidateFFI,
    _max_items: u32,
) -> u32 {
    // Phase 3: populate from pinyin engine.
    // For now (Phase 1), snippets match uniquely — no candidate windows needed.
    0
}

/// Engine activated (user switched to this input method).
#[no_mangle]
pub extern "C" fn swift_ime_activate() {
    tracing::debug!("fcitx5 activate");
}

/// Engine deactivated (user switched away).
#[no_mangle]
pub extern "C" fn swift_ime_deactivate() {
    tracing::debug!("fcitx5 deactivate");
}

/// Reset engine state (Escape, focus change).
#[no_mangle]
pub extern "C" fn swift_ime_reset() {
    let mut guard = STATE.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        state.dispatcher.reset(&mut state.ime_state);
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn default_store() -> SnippetStore {
    SnippetStore::from_json(DEFAULT_SNIPPETS).unwrap_or_else(|_| SnippetStore::new())
}

fn chrono_date() -> String {
    // Lightweight date — avoid pulling in chrono just for this.
    // For Phase 1, use a static date. Phase 2 adds real date via chrono.
    "2026-07-23".into()
}

const DEFAULT_SNIPPETS: &str = r##"[
    {"trigger": "/greet", "expand": "你好，我是 AI 秘书，请问有什么可以帮你的？", "desc": "通用问候语"},
    {"trigger": "/sig", "expand": "Best regards,\nAlice\n$DATE", "desc": "邮件签名"}
]"##;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn init_sets_up_state() {
        let path = CString::new("").unwrap();
        assert_eq!(swift_ime_init(path.as_ptr()), 0);
        assert!(STATE.lock().unwrap().is_some());
    }

    #[test]
    fn process_key_returns_text() {
        let path = CString::new("").unwrap();
        swift_ime_init(path.as_ptr());

        let mut buf = vec![0u8; 256];
        let mut len: u32 = 0;

        // '/' → preedit
        let a = swift_ime_process_key('/' as u32, buf.as_mut_ptr(), 256, &mut len);
        assert_eq!(a, ImeActionFFI::Preedit);
        assert_eq!(unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }.to_str().unwrap(), "/");
    }

    #[test]
    fn reset_clears_state() {
        let path = CString::new("").unwrap();
        swift_ime_init(path.as_ptr());
        swift_ime_process_key('/' as u32, std::ptr::null_mut(), 0, std::ptr::null_mut());
        swift_ime_reset();
        // After reset, new '/' starts fresh.
        let mut buf = vec![0u8; 256];
        let mut len: u32 = 0;
        let a = swift_ime_process_key('/' as u32, buf.as_mut_ptr(), 256, &mut len);
        assert_eq!(a, ImeActionFFI::Preedit);
    }
}
