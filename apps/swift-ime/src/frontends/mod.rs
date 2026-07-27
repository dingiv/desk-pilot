//! Platform adapter modules. Each OS IME backend (fcitx5, ibus, TSF, IMK) has its own module.
//!
//! The fcitx5 frontend uses the ImeView C ABI — `#[repr(C)]` structs passed by pointer.
//! Other backends implement `PlatformAdapter` (ibus via DBus, TSF via COM, IMK via AppKit).

pub mod fcitx5;
pub mod ibus;
pub mod tsf;
pub mod imk;

// Re-export ImeView types from ime-core — no more ImeActionFFI/CandidateFFI wrappers.
pub use ime_core::{ImeView, CandidateSlot, CANDIDATE_SLOTS};

/// Per-platform adapter interface.
///
/// Lifecycle: `activate()` → N × `process_key()` → `deactivate()`. `reset()` can
/// fire at any time (focus change, Escape). The adapter owns the `Dispatcher` and
/// `StateMachine`, and may have platform-specific fields (e.g. dbus connection handle,
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
    /// Process a key event. `ch` is the Unicode character. Returns the full ImeView snapshot.
    fn process_key(&mut self, ch: char) -> ImeView;
    /// User selected a candidate from the popup.
    fn select_candidate(&mut self, index: usize) -> ImeView;
}
