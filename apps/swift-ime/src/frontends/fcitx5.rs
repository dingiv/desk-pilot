//! fcitx5 frontend — C ABI entry points for the cdylib.
//!
//! Architecture: one opaque `ImeHandle` per `SwiftImeEngine` instance.
//! The handle owns the Dispatcher and per-context StateMachine map —
//! no global statics. The C++ side creates/destroys the handle and
//! passes it on every call.

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::Mutex;
use std::time::Instant;

use ime_core::{
    Dispatcher, Expander, Matcher, SnippetStore,
    expander::StaticProvider,
    platform::ImeView,
    state::StateMachine,
};

// ── ImeHandle — opaque, owned by SwiftImeEngine ──────────────────────────

pub struct ImeHandle {
    dispatcher: Dispatcher,
    contexts: Mutex<HashMap<usize, StateMachine>>,
    async_waits: Mutex<HashMap<usize, WaitState>>,
}

struct WaitState {
    trigger_time: Instant,
    chars: Vec<(u64, char)>,
}

impl ImeHandle {
    fn new(config_path: Option<&CStr>) -> Self {
        let store = config_path
            .map(|p| p.to_string_lossy().into_owned())
            .and_then(|s| SnippetStore::from_json(&s).ok())
            .unwrap_or_else(|| SnippetStore::new());

        let matcher = Matcher::new(store.entries());
        let expander = Expander::new(Box::new(StaticProvider {
            date: String::from("2026-07-23"),
            clipboard: String::new(),
        }));
        tracing::info!(snippets = store.len(), "ImeHandle created");

        ImeHandle {
            dispatcher: Dispatcher::new(matcher, expander),
            contexts: Mutex::new(HashMap::new()),
            async_waits: Mutex::new(HashMap::new()),
        }
    }

    // ── ctx accessors ─────────────────────────────────────────────────
    fn with_ctx<T>(
        &self,
        ctx: *const std::ffi::c_void,
        f: impl FnOnce(&Dispatcher, &mut StateMachine) -> T,
    ) -> T {
        let mut map = self.contexts.lock().unwrap();
        let sm = map.entry(ctx as usize).or_default();
        f(&self.dispatcher, sm)
    }

    // ── operations ────────────────────────────────────────────────────

    fn process_key(&self, ctx: *const std::ffi::c_void, ch: char) -> ImeView {
        let mut view = self.with_ctx(ctx, |disp, sm| disp.process_key(ch, sm));
        if ImeView::str_field(&view.commit_text) == "__WAIT_DEMO__" {
            self.async_waits.lock().unwrap().insert(
                ctx as usize,
                WaitState {
                    trigger_time: Instant::now(),
                    chars: vec![(0, 'a'), (1000, 'b'), (2000, 'c')],
                },
            );
            view.commit_text = [0u8; 512];
            ImeView::set_str(&mut view.preedit_text, "a");
            view.preedit_cursor = 1;
        }
        view
    }

    fn select_candidate(&self, ctx: *const std::ffi::c_void, index: usize) -> ImeView {
        self.with_ctx(ctx, |disp, sm| disp.select_candidate(index, sm))
    }

    fn reset(&self, ctx: *const std::ffi::c_void) {
        self.with_ctx(ctx, |disp, sm| disp.reset(sm));
    }

    fn activate(&self, _ctx: *const std::ffi::c_void) { /* no-op */ }

    fn deactivate(&self, ctx: *const std::ffi::c_void) {
        let k = ctx as usize;
        self.contexts.lock().unwrap().remove(&k);
        self.async_waits.lock().unwrap().remove(&k);
    }

    fn commit_pending(&self, ctx: *const std::ffi::c_void) -> ImeView {
        let map = self.contexts.lock().unwrap();
        let text = map
            .get(&(ctx as usize))
            .map(|sm| {
                sm.candidates
                    .first()
                    .cloned()
                    .unwrap_or_else(|| sm.buffer.clone())
            });
        let mut v = ImeView::empty();
        if let Some(t) = text {
            if !t.is_empty() {
                ImeView::set_str(&mut v.commit_text, &t);
            }
        }
        v
    }

    fn poll_async(&self, ctx: *const std::ffi::c_void) -> (i32, ImeView) {
        let mut waits = self.async_waits.lock().unwrap();
        let Some(ws) = waits.get(&(ctx as usize)) else {
            return (0, ImeView::empty());
        };
        let ms = ws.trigger_time.elapsed().as_millis() as u64;
        let text: String = ws
            .chars
            .iter()
            .filter(|(t, _)| *t <= ms)
            .map(|(_, c)| *c)
            .collect();
        let mut v = ImeView::empty();
        if ms > 2100 {
            waits.remove(&(ctx as usize));
            ImeView::set_str(&mut v.commit_text, &text);
            (2, v)
        } else {
            ImeView::set_str(&mut v.preedit_text, &text);
            v.preedit_cursor = text.len() as u32;
            (1, v)
        }
    }
}

// ── C ABI — all functions take the handle pointer ──────────────────────

#[no_mangle]
pub extern "C" fn swift_ime_create(config_path: *const c_char) -> *mut ImeHandle {
    let path = if config_path.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(config_path) })
    };
    Box::into_raw(Box::new(ImeHandle::new(path)))
}

#[no_mangle]
pub extern "C" fn swift_ime_destroy(handle: *mut ImeHandle) {
    if handle.is_null() {
        return;
    }
    unsafe {
        drop(Box::from_raw(handle));
    }
}

#[no_mangle]
pub extern "C" fn swift_ime_process_key(
    handle: *mut ImeHandle,
    ctx: *const std::ffi::c_void,
    ch: u32,
    out_view: *mut ImeView,
) -> i32 {
    let c = char::from_u32(ch).unwrap_or('\0');
    if c == '\0' || handle.is_null() || out_view.is_null() {
        return 0;
    }
    let view = unsafe { &*handle }.process_key(ctx, c);
    unsafe {
        *out_view = view;
    }
    1
}

#[no_mangle]
pub extern "C" fn swift_ime_select_candidate(
    handle: *mut ImeHandle,
    ctx: *const std::ffi::c_void,
    index: u32,
    out_view: *mut ImeView,
) -> i32 {
    if handle.is_null() || out_view.is_null() {
        return 0;
    }
    let view = unsafe { &*handle }.select_candidate(ctx, index as usize);
    unsafe {
        *out_view = view;
    }
    1
}

#[no_mangle]
pub extern "C" fn swift_ime_reset(handle: *mut ImeHandle, ctx: *const std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    unsafe { &*handle }.reset(ctx);
}

#[no_mangle]
pub extern "C" fn swift_ime_activate(handle: *mut ImeHandle, ctx: *const std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    unsafe { &*handle }.activate(ctx);
}

#[no_mangle]
pub extern "C" fn swift_ime_deactivate(handle: *mut ImeHandle, ctx: *const std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    unsafe { &*handle }.deactivate(ctx);
}

#[no_mangle]
pub extern "C" fn swift_ime_commit_pending(
    handle: *mut ImeHandle,
    ctx: *const std::ffi::c_void,
    out_view: *mut ImeView,
) -> i32 {
    if handle.is_null() || out_view.is_null() {
        return 0;
    }
    let view = unsafe { &*handle }.commit_pending(ctx);
    unsafe {
        *out_view = view;
    }
    1
}

#[no_mangle]
pub extern "C" fn swift_ime_poll_async(
    handle: *mut ImeHandle,
    ctx: *const std::ffi::c_void,
    out_view: *mut ImeView,
) -> i32 {
    if handle.is_null() || out_view.is_null() {
        return 0;
    }
    let (code, view) = unsafe { &*handle }.poll_async(ctx);
    if code != 0 {
        unsafe {
            *out_view = view;
        }
    }
    code
}
