//! Platform adapter trait. Each OS IME backend (fcitx5, ibus, TSF, IMK) implements

pub mod fcitx5;
pub mod ibus;
pub mod tsf;
pub mod imk;

// C ABI types and constants — defined in the library target (src/lib.rs → ffi module).
pub use swift_ime::ffi::{ImeActionFFI, CandidateFFI, MAX_CANDIDATES};

use ime_core::ImeAction;

/// Translate `ime_core::ImeAction` into the C ABI version plus an optional
/// string buffer (commit / preedit text).
pub fn translate_action(action: ImeAction) -> (ImeActionFFI, String) {
    match action {
        ImeAction::PassThrough => (ImeActionFFI::PassThrough, String::new()),
        ImeAction::Preedit { text, .. } => (ImeActionFFI::Preedit, text),
        ImeAction::Commit(text) => (ImeActionFFI::Commit, text),
        ImeAction::Candidates { .. } => (ImeActionFFI::Candidates, String::new()),
    }
}

/// Per-platform adapter interface.
///
/// Lifecycle: `activate()` → N × `process_key()` → `deactivate()`. `reset()` can
/// fire at any time (focus change, Escape). The adapter owns the `Dispatcher` and
/// `ImeState`, and may have platform-specific fields (e.g. dbus connection handle,
/// win32 composition window handle, etc.).
///
/// Implementations live in per-platform modules:
/// - `fcitx5.rs`  — Linux fcitx5 addon (priority, via C++ thin glue calling our C ABI)
/// - `ibus.rs`    — Linux ibus DBus engine (Phase 4)
/// - `tsf.rs`     — Windows TSF COM text service (Phase 5)
/// - `imk.rs`     — macOS IMK input controller (Phase 5)
pub trait PlatformAdapter: Send {
    /// The engine was activated (user switched to it, or input context gained focus).
    fn activate(&mut self);
    /// The engine was deactivated (user switched away, or context lost focus).
    fn deactivate(&mut self);
    /// Reset engine state (Escape, focus change).
    fn reset(&mut self);
    /// Process a key event. `ch` is the Unicode character, `modifiers` is a
    /// platform-specific bitmask. Returns the action the platform should execute.
    fn process_key(&mut self, ch: char) -> ImeAction;
    /// User selected a candidate from the popup.
    fn select_candidate(&mut self, index: usize) -> ImeAction;
}
